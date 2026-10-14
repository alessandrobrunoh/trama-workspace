//! Copy materialization: copy-on-write clones, git clone fallback and
//! repo-local helpers.

use super::*;

impl CheckoutManager {
    /// Copy `origin` (a source repository or a parent checkout) to the new
    /// directory `staging`: a copy-on-write clone of the whole working tree
    /// when possible, else `git clone`. Never modifies `origin`; on error
    /// nothing is left at `staging`.
    pub(crate) fn materialize(
        &self,
        origin: &Path,
        staging: &Path,
    ) -> Result<CopyMethod, CheckoutError> {
        if self.copy.mode == CopyMode::CopyOnWrite && cow_eligible(origin) {
            fs::create_dir(staging)?;
            let skip_trama = !trama_is_config_only(origin);
            let excluded: Vec<&str> = self
                .copy
                .exclude
                .iter()
                .map(|entry| entry.trim().trim_end_matches('/'))
                .filter(|entry| !entry.is_empty() && !entry.contains('/'))
                .collect();
            let skip = |name: &OsStr| {
                (skip_trama && name == ".trama")
                    || excluded.iter().any(|entry| OsStr::new(entry) == name)
            };
            let result = if self.copy.simulate_clone_failure {
                Err(crate::fs::cow::CloneError::Unsupported(
                    std::io::Error::from(std::io::ErrorKind::Unsupported),
                ))
            } else {
                crate::fs::cow::clone_tree(origin, staging, &skip)
            };
            match result {
                Ok(stats)
                    if fs::symlink_metadata(staging.join(".git"))
                        .is_ok_and(|metadata| metadata.is_dir())
                        && !stats.skipped.iter().any(|path| path.starts_with(".git")) =>
                {
                    fixup_cow_git(staging)?;
                    return Ok(CopyMethod::CopyOnWrite);
                }
                _ => {
                    // Not APFS, another volume, or an unreadable `.git`:
                    // fall back to Git below.
                    fs::remove_dir_all(staging)?;
                }
            }
        }
        // Without `--no-hardlinks`: Git hardlinks the immutable object files
        // of a local clone instead of duplicating history.
        git(
            origin,
            [
                OsStr::new("clone"),
                OsStr::new("--quiet"),
                OsStr::new("--"),
                origin.as_os_str(),
                staging.as_os_str(),
            ],
        )?;
        Ok(CopyMethod::GitClone)
    }

    /// Make a fresh copy of `source` an independent checkout of the source's
    /// `HEAD` with a clean tree (no tracked changes, no untracked non-ignored
    /// files), `local` pointing at `source` and `origin` at the source's
    /// `origin` (the source itself when it has none).
    pub(crate) fn finish_source_copy(
        &self,
        source: &Path,
        staging: &Path,
        method: CopyMethod,
    ) -> Result<(), CheckoutError> {
        let source_origin = git_text(source, ["remote", "get-url", "origin"]).ok();
        match method {
            CopyMethod::GitClone => {
                if source_origin.is_some() {
                    // The clone's `origin/*` are the source's branches:
                    // keep them as `local/*` and point `origin` upstream.
                    git(staging, ["remote", "rename", "origin", "local"])?;
                }
                set_remote(staging, "local", Some(source.as_os_str()))?;
                let origin = source_origin
                    .as_deref()
                    .map(OsStr::new)
                    .unwrap_or(source.as_os_str());
                set_remote(staging, "origin", Some(origin))?;
            }
            CopyMethod::CopyOnWrite => {
                set_remote(staging, "local", Some(source.as_os_str()))?;
                if source_origin.is_none() {
                    set_remote(staging, "origin", Some(source.as_os_str()))?;
                }
                delete_trama_refs(staging)?;
                clean_to_head(staging)?;
                self.prune_copy(staging)?;
            }
        }
        Ok(())
    }

    /// Remove from a copy-on-write copy what must not be cloned: paths the
    /// repository links to the primary checkout (setup links them back) and
    /// nested `exclude` entries. Only ignored, untracked paths are removed.
    pub(crate) fn prune_copy(&self, staging: &Path) -> Result<(), CheckoutError> {
        let mut entries = crate::process::setup::linked_entries(staging);
        entries.extend(
            self.copy
                .exclude
                .iter()
                .map(|entry| entry.trim().trim_end_matches('/').to_owned())
                .filter(|entry| entry.contains('/')),
        );
        for entry in entries {
            let Some(path) = plain_child(staging, &entry) else {
                continue;
            };
            if fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let tracked = git(staging, ["ls-files", "-z", "--", entry.as_str()])?;
            if !tracked.stdout.is_empty() {
                continue;
            }
            if git(staging, ["check-ignore", "-q", "--", entry.as_str()]).is_err() {
                continue;
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path)?,
                Ok(_) => fs::remove_file(&path)?,
                Err(_) => {}
            }
        }
        Ok(())
    }
}

/// `root/<entry>` when `entry` is a plain relative path outside `.git` and
/// `.trama` whose parents inside `root` are real directories.
pub(super) fn plain_child(root: &Path, entry: &str) -> Option<PathBuf> {
    use std::path::Component;
    if entry.is_empty() || entry.starts_with(':') || entry.contains(['*', '?', '[', '\\']) {
        return None;
    }
    let mut parts = Vec::new();
    for component in Path::new(entry).components() {
        match component {
            Component::Normal(part) => parts.push(part),
            _ => return None,
        }
    }
    let first = parts.first()?;
    if *first == ".trama" || parts.iter().any(|part| *part == ".git") {
        return None;
    }
    let mut current = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        current.push(part);
        let metadata = fs::symlink_metadata(&current).ok()?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return None;
        }
    }
    Some(root.join(entry))
}

/// A tree can be cloned whole only when its `.git` is a self-contained
/// directory: not a gitfile (linked worktree, submodule), no `commondir`,
/// no alternates (relative paths would break), no `core.worktree`.
pub(super) fn cow_eligible(origin: &Path) -> bool {
    let dot_git = origin.join(".git");
    fs::symlink_metadata(&dot_git).is_ok_and(|metadata| metadata.is_dir())
        && fs::symlink_metadata(dot_git.join("commondir")).is_err()
        && fs::symlink_metadata(dot_git.join("objects/info/alternates")).is_err()
        && git(origin, ["config", "--local", "--get", "core.worktree"]).is_err()
}

/// True when `<dir>/.trama` holds only the tracked setup overrides, so it is
/// part of the checkout rather than Trama's managed storage.
pub(super) fn trama_is_config_only(dir: &Path) -> bool {
    let trama = dir.join(".trama");
    if !fs::symlink_metadata(&trama).is_ok_and(|metadata| metadata.is_dir()) {
        return false;
    }
    fs::read_dir(&trama).is_ok_and(|entries| {
        entries.into_iter().all(|entry| {
            entry.is_ok_and(|entry| {
                entry.file_type().is_ok_and(|kind| kind.is_file())
                    && matches!(entry.file_name().to_str(), Some("prepare" | "linked"))
            })
        })
    })
}

/// Drop state a cloned `.git` must not inherit: locks and operations in
/// progress in the original, its linked-worktree registry (whose entries
/// point at the original's worktrees), and Trama's per-checkout records.
pub(super) fn fixup_cow_git(staging: &Path) -> Result<(), CheckoutError> {
    let git_dir = staging.join(".git");
    for name in [
        "worktrees",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "MERGE_HEAD",
        "MERGE_MODE",
        "MERGE_MSG",
        "AUTO_MERGE",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "BISECT_START",
        "BISECT_TERMS",
        "BISECT_EXPECTED_REV",
        "BISECT_ANCESTORS_OK",
        "BISECT_NAMES",
        "gc.pid",
        "trama-base-commit",
        crate::process::setup::RECORD_FILE,
        crate::process::setup::LOG_FILE,
    ] {
        let path = git_dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path)?,
            Ok(_) => fs::remove_file(&path)?,
            Err(_) => {}
        }
    }
    for entry in fs::read_dir(&git_dir)? {
        let entry = entry?;
        if entry.file_name().as_encoded_bytes().ends_with(b".lock") && entry.file_type()?.is_file()
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Add, retarget or (with `None`) remove remote `name`.
pub(super) fn set_remote(
    checkout: &Path,
    name: &str,
    url: Option<&OsStr>,
) -> Result<(), CheckoutError> {
    let remotes = git_text(checkout, ["remote"])?;
    let exists = remotes.lines().any(|line| line.trim() == name);
    match (url, exists) {
        (Some(url), true) => {
            git(
                checkout,
                [
                    OsStr::new("remote"),
                    OsStr::new("set-url"),
                    OsStr::new(name),
                    url,
                ],
            )?;
        }
        (Some(url), false) => {
            git(
                checkout,
                [
                    OsStr::new("remote"),
                    OsStr::new("add"),
                    OsStr::new(name),
                    url,
                ],
            )?;
        }
        (None, true) => {
            git(checkout, ["remote", "remove", name])?;
        }
        (None, false) => {}
    }
    Ok(())
}

/// Delete every `refs/trama/*` ref (objects stay until Git prunes them).
pub(crate) fn delete_trama_refs(checkout: &Path) -> Result<(), CheckoutError> {
    let refs = git_text(
        checkout,
        ["for-each-ref", "--format=%(refname)", "refs/trama/"],
    )?;
    for reference in refs.lines().filter(|line| !line.is_empty()) {
        git(checkout, ["update-ref", "-d", reference])?;
    }
    Ok(())
}

/// Refresh the copied index (its stat data describes the original's files)
/// without touching files, then make the tree exactly `HEAD` when anything
/// differs: tracked changes reverted, untracked non-ignored files removed.
/// Ignored files (build output) are kept, and unchanged files keep their
/// modification times so incremental builds stay valid.
pub(crate) fn clean_to_head(checkout: &Path) -> Result<(), CheckoutError> {
    let _ = git(checkout, ["update-index", "-q", "--refresh"]);
    let status = ["status", "--porcelain", "--untracked-files=all"];
    if !git(checkout, status)?.stdout.is_empty() {
        git(checkout, ["reset", "--quiet", "--hard"])?;
        git(checkout, ["clean", "-fdq"])?;
        if !git(checkout, status)?.stdout.is_empty() {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
    }
    Ok(())
}

/// Repository-tracked files allowed under `.trama` (see [`crate::process::setup`]).
pub(super) const TRACKED_TRAMA_CONFIG: [&[u8]; 2] = [b".trama/prepare", b".trama/linked"];

pub(super) fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub(super) fn ensure_local_exclude(source: &Path) -> Result<(), CheckoutError> {
    let value = git_text(source, ["rev-parse", "--git-path", "info/exclude"])?;
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        source.join(path)
    };
    let existing = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CheckoutError::Io(error)),
    };
    if existing.lines().any(|line| line.trim() == "/.trama/") {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.write_all(b"/.trama/\n")?;
    Ok(())
}

/// Canonicalize `path` through its deepest ancestor that already exists, so a
/// path that has not been created yet still compares truthfully (same resolved
/// prefixes) against fully existing paths such as a source repository.
pub(super) fn canonicalize_deepest_existing(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return Ok(canonical);
    }
    let mut missing = Vec::new();
    let mut existing = path.to_path_buf();
    while !existing.exists() {
        let segment = existing
            .file_name()
            .map(|name| name.to_os_string())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} has no existing ancestor", path.display()),
                )
            })?;
        missing.push(segment);
        existing = existing.parent().map(Path::to_path_buf).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} has no existing ancestor", path.display()),
            )
        })?;
    }
    let mut canonical = dunce::canonicalize(&existing)?;
    while let Some(segment) = missing.pop() {
        canonical.push(segment);
    }
    Ok(canonical)
}

pub(crate) fn git<I, S>(cwd: &Path, args: I) -> Result<Output, CheckoutError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().collect();
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(&args)
        .output()?;
    if !output.status.success() {
        return Err(CheckoutError::Git {
            args: args
                .iter()
                .map(|arg| arg.as_ref().to_string_lossy().into_owned())
                .collect(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output)
}

pub(crate) fn text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub(crate) fn git_text<I, S>(cwd: &Path, args: I) -> Result<String, CheckoutError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Ok(text(git(cwd, args)?))
}

pub(super) fn classify_non_worktree(path: &Path) -> CheckoutError {
    match git_text(path, ["rev-parse", "--is-bare-repository"]) {
        Ok(value) if value == "true" => CheckoutError::Bare {
            path: path.to_path_buf(),
        },
        _ => CheckoutError::NotWorktree {
            path: path.to_path_buf(),
        },
    }
}

pub(super) fn optional_origin(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if url.is_empty() {
        return None;
    }
    let path = Path::new(&url);
    if path.is_absolute() {
        if let Ok(canonical) = dunce::canonicalize(path) {
            return Some(canonical.to_string_lossy().into_owned());
        }
    }
    Some(url)
}
