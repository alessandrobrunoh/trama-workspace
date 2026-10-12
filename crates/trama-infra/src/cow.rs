//! Copy-on-write directory clones and real disk accounting (macOS APFS).
//!
//! [`clone_tree`] clones a directory's entries with `clonefile(2)`: data
//! blocks stay shared with the original until either side writes, so a clone
//! of a 20 GB checkout (build output included) costs metadata only.
//! [`disk_usage`] reports both what `du` shows (allocated size, blind to
//! sharing) and what deleting a tree would actually free: the blocks of each
//! file that no clone shares (`ATTR_CMNEXT_PRIVATESIZE`).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Why a tree could not be cloned copy-on-write. `Unsupported` (no
/// `clonefile`, a volume without clones, a cross-volume destination) means
/// the caller should fall back to another copy method.
#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    #[error("copy-on-write clones are not supported here: {0}")]
    Unsupported(io::Error),
    #[error("copy-on-write clone failed: {0}")]
    Io(#[from] io::Error),
}

/// What [`clone_tree`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloneStats {
    /// Entries cloned with one `clonefile` call each (a directory counts once).
    pub cloned: usize,
    /// Entries `clonefile` refused, relative to the source, that were skipped
    /// (unreadable files, files that vanished during the copy).
    pub skipped: Vec<PathBuf>,
}

/// Clone every entry of `source` into the existing empty directory
/// `destination`, except top-level names for which `skip` returns true.
/// Directories are cloned whole; when the kernel refuses one, it is recreated
/// and its children are cloned one by one. Symbolic links are cloned as links
/// and never followed. `source` is only read.
pub fn clone_tree(
    source: &Path,
    destination: &Path,
    skip: &dyn Fn(&OsStr) -> bool,
) -> Result<CloneStats, CloneError> {
    let mut stats = CloneStats::default();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip(&name) {
            continue;
        }
        clone_entry(
            &source.join(&name),
            &destination.join(&name),
            Path::new(&name),
            &mut stats,
        )?;
    }
    Ok(stats)
}

fn clone_entry(
    source: &Path,
    destination: &Path,
    relative: &Path,
    stats: &mut CloneStats,
) -> Result<(), CloneError> {
    match clonefile(source, destination) {
        Ok(()) => {
            stats.cloned += 1;
            Ok(())
        }
        Err(error) if is_unsupported(&error) => Err(CloneError::Unsupported(error)),
        Err(_) => {
            let metadata = match fs::symlink_metadata(source) {
                Ok(metadata) => metadata,
                Err(_) => {
                    // Vanished while copying.
                    stats.skipped.push(relative.to_path_buf());
                    return Ok(());
                }
            };
            if !metadata.is_dir() {
                let _ = fs::remove_file(destination);
                stats.skipped.push(relative.to_path_buf());
                return Ok(());
            }
            // A partial directory clone may have been left behind.
            if fs::symlink_metadata(destination).is_ok() {
                fs::remove_dir_all(destination)?;
            }
            fs::create_dir(destination)?;
            fs::set_permissions(destination, metadata.permissions())?;
            let Ok(children) = fs::read_dir(source) else {
                stats.skipped.push(relative.to_path_buf());
                return Ok(());
            };
            for child in children {
                let child = child?;
                let name = child.file_name();
                clone_entry(
                    &source.join(&name),
                    &destination.join(&name),
                    &relative.join(&name),
                    stats,
                )?;
            }
            Ok(())
        }
    }
}

fn is_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            error.raw_os_error(),
            Some(libc::ENOTSUP) | Some(libc::EXDEV) | Some(libc::ENOSYS)
        )
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn clonefile(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    /// `CLONE_NOFOLLOW | CLONE_NOOWNERCOPY` from `<sys/clonefile.h>`.
    const FLAGS: u32 = 0x0001 | 0x0002;
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated paths.
    let status = unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), FLAGS) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn clonefile(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Disk used by a directory tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Allocated bytes of every file, counting shared blocks fully (`du`).
    pub apparent_bytes: u64,
    /// Bytes that removing the tree would free: blocks not shared with any
    /// clone. Hard-linked files count their size divided by the link count.
    pub private_bytes: u64,
    /// True when the volume could not report private sizes for some files
    /// (or hard links were found); those files count fully.
    pub estimated: bool,
    pub files: u64,
}

impl DiskUsage {
    fn add(&mut self, other: DiskUsage) {
        self.apparent_bytes += other.apparent_bytes;
        self.private_bytes += other.private_bytes;
        self.estimated |= other.estimated;
        self.files += other.files;
    }
}

/// Measure `root` without following symbolic links. Unreadable directories
/// are skipped.
pub fn disk_usage(root: &Path) -> io::Result<DiskUsage> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() {
        return Ok(file_usage_fallback(&metadata));
    }
    let mut total = DiskUsage::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        match scan_dir(&dir, &mut pending) {
            Ok(usage) => total.add(usage),
            Err(_) if dir != root => {}
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

#[cfg(unix)]
fn file_usage_fallback(metadata: &fs::Metadata) -> DiskUsage {
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file() {
        return DiskUsage::default();
    }
    let allocated = metadata.blocks() * 512;
    DiskUsage {
        apparent_bytes: allocated,
        private_bytes: allocated / metadata.nlink().max(1),
        estimated: true,
        files: 1,
    }
}

#[cfg(not(unix))]
fn file_usage_fallback(metadata: &fs::Metadata) -> DiskUsage {
    if !metadata.is_file() {
        return DiskUsage::default();
    }
    DiskUsage {
        apparent_bytes: metadata.len(),
        private_bytes: metadata.len(),
        estimated: true,
        files: 1,
    }
}

/// Portable scan: `lstat` sizes, no sharing information.
fn scan_dir_portable(dir: &Path, pending: &mut Vec<PathBuf>) -> io::Result<DiskUsage> {
    let mut usage = DiskUsage::default();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            pending.push(entry.path());
        } else {
            usage.add(file_usage_fallback(&metadata));
        }
    }
    Ok(usage)
}

#[cfg(not(target_os = "macos"))]
fn scan_dir(dir: &Path, pending: &mut Vec<PathBuf>) -> io::Result<DiskUsage> {
    scan_dir_portable(dir, pending)
}

/// One `getattrlistbulk` pass over `dir`: name, type, link count, allocated
/// size and private size of every entry.
#[cfg(target_os = "macos")]
fn scan_dir(dir: &Path, pending: &mut Vec<PathBuf>) -> io::Result<DiskUsage> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;

    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    let handle = fs::File::open(dir)?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
    };
    let options = (libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_PACK_INVAL_ATTRS) as u64;
    let mut buffer = vec![0u8; 128 * 1024];
    let mut usage = DiskUsage::default();
    let mut first = true;
    loop {
        // SAFETY: `list` and `buffer` are valid for the duration of the call
        // and `buffer.len()` is its size.
        let count = unsafe {
            libc::getattrlistbulk(
                handle.as_raw_fd(),
                (&mut list as *mut libc::attrlist).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                options,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if !first {
                return Err(error);
            }
            // The volume cannot answer in bulk: measure this directory
            // without sharing information.
            return scan_dir_portable(dir, pending);
        }
        first = false;
        if count == 0 {
            break;
        }
        let mut offset = 0usize;
        for _ in 0..count {
            let entry = &buffer[offset..];
            let length = read_u32(entry, 0) as usize;
            if length == 0 || offset + length > buffer.len() {
                return Err(io::Error::other("malformed getattrlistbulk reply"));
            }
            // attribute_set_t: common, vol, dir, file, fork.
            let returned_file = read_u32(entry, 4 + 12);
            let returned_fork = read_u32(entry, 4 + 16);
            let mut field = 4 + 20;
            let name_offset = read_u32(entry, field) as i32;
            let name_length = read_u32(entry, field + 4) as usize;
            let name_start = (field as i64 + name_offset as i64) as usize;
            field += 8;
            let object_type = read_u32(entry, field);
            field += 4;
            let link_count = read_u32(entry, field);
            field += 4;
            let allocated = read_u64(entry, field);
            field += 8;
            let private = read_u64(entry, field);
            let name_bytes = &entry[name_start..name_start + name_length.saturating_sub(1)];
            let name = OsStr::from_bytes(name_bytes);
            match object_type {
                VDIR => {
                    if name != "." && name != ".." {
                        pending.push(dir.join(name));
                    }
                }
                VREG => {
                    usage.files += 1;
                    usage.apparent_bytes += allocated;
                    let has_private = returned_fork & libc::ATTR_CMNEXT_PRIVATESIZE != 0;
                    let has_links = returned_file & libc::ATTR_FILE_LINKCOUNT != 0;
                    if has_links && link_count > 1 {
                        usage.private_bytes += allocated / u64::from(link_count);
                        usage.estimated = true;
                    } else if has_private {
                        usage.private_bytes += private;
                    } else {
                        usage.private_bytes += allocated;
                        usage.estimated = true;
                    }
                }
                _ => {}
            }
            offset += length;
        }
    }
    Ok(usage)
}

#[cfg(target_os = "macos")]
fn read_u32(buffer: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(buffer[at..at + 4].try_into().expect("4 bytes"))
}

#[cfg(target_os = "macos")]
fn read_u64(buffer: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(buffer[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_random(path: &Path, bytes: usize) {
        let mut file = fs::File::create(path).unwrap();
        let mut state = 0x9e3779b97f4a7c15u64 ^ bytes as u64;
        let mut chunk = vec![0u8; 64 * 1024];
        let mut left = bytes;
        while left > 0 {
            for byte in chunk.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            let take = left.min(chunk.len());
            file.write_all(&chunk[..take]).unwrap();
            left -= take;
        }
        file.sync_all().unwrap();
    }

    #[test]
    fn clone_tree_shares_blocks_and_skips_names() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("big/nested")).unwrap();
        fs::create_dir_all(source.join(".trama/worktrees")).unwrap();
        write_random(&source.join("big/nested/blob"), 4 << 20);
        fs::write(source.join(".trama/worktrees/secret"), "never copied").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", source.join("link")).unwrap();
        let destination = temp.path().join("copy");
        fs::create_dir(&destination).unwrap();
        let stats = clone_tree(&source, &destination, &|name| name == ".trama").unwrap();
        assert_eq!(stats.cloned, 2);
        assert!(stats.skipped.is_empty());
        assert!(!destination.join(".trama").exists());
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("/etc/hosts")
        );
        let usage = disk_usage(&destination).unwrap();
        assert_eq!(usage.files, 1);
        assert!(usage.apparent_bytes >= 4 << 20, "{usage:?}");
        assert_eq!(usage.private_bytes, 0, "{usage:?}");
        assert!(!usage.estimated);

        // Rewriting 64 KiB of the clone only makes those blocks private.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(destination.join("big/nested/blob"))
            .unwrap();
        file.write_all(&vec![7u8; 64 * 1024]).unwrap();
        file.sync_all().unwrap();
        let usage = disk_usage(&destination).unwrap();
        assert!(usage.private_bytes >= 64 * 1024, "{usage:?}");
        assert!(usage.private_bytes < 1 << 20, "{usage:?}");
        // The original is untouched.
        assert_eq!(
            fs::read(source.join(".trama/worktrees/secret")).unwrap(),
            b"never copied"
        );
    }

    #[test]
    fn hard_links_are_split_and_flagged_estimated() {
        let temp = tempfile::tempdir().unwrap();
        write_random(&temp.path().join("a"), 256 * 1024);
        fs::hard_link(temp.path().join("a"), temp.path().join("b")).unwrap();
        let usage = disk_usage(temp.path()).unwrap();
        assert_eq!(usage.files, 2);
        assert!(usage.estimated);
        assert_eq!(usage.private_bytes, usage.apparent_bytes / 2);
    }
}
