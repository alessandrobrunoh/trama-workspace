//! Working-tree snapshots, diffs and subthread pulls on managed checkouts.
//!
//! Snapshots use a throwaway index (`GIT_INDEX_FILE`) so the checkout's real
//! index, working tree and branches are never touched. They capture tracked
//! changes (staged or not) and untracked non-ignored files; ignored files are
//! never included. Snapshot objects are written to the checkout's object
//! store and kept alive by refs under `refs/trama/`.

use crate::checkout::{CheckoutError, git, git_text, text};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use trama_domain::subthread::{FileChange, FileChangeStatus, PullFileOutcome, PullFileResult};

/// Snapshot of a subthread copy right after it was forked from its parent.
pub const FORK_POINT_REF: &str = "refs/trama/fork-point";
/// Snapshot of a thread's checkout taken when its latest agent turn started.
pub const LAST_TURN_REF: &str = "refs/trama/last-turn";
/// Diff text above this size is truncated (stats stay complete).
pub const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;

const SNAPSHOT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Trama"),
    ("GIT_AUTHOR_EMAIL", "trama@localhost"),
    ("GIT_COMMITTER_NAME", "Trama"),
    ("GIT_COMMITTER_EMAIL", "trama@localhost"),
];

/// A unified diff of a checkout's current working state against a base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoDiff {
    /// `git diff --no-color -M` text (binary files are summarized, not inlined).
    pub diff: String,
    pub truncated: bool,
    pub files: Vec<FileChange>,
}

/// Removes a temporary file or directory on drop.
struct TempPath(PathBuf);

impl TempPath {
    fn new(prefix: &str) -> Self {
        Self(std::env::temp_dir().join(format!("{prefix}-{}", ulid::Ulid::new())))
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = fs::remove_dir_all(&self.0);
        } else {
            let _ = fs::remove_file(&self.0);
        }
    }
}

/// Run git without failing on a non-zero exit.
fn run_git<I, S>(
    cwd: &Path,
    args: I,
    envs: &[(&str, &OsStr)],
    stdin: Option<&[u8]>,
) -> Result<Output, CheckoutError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    if let Some(input) = stdin {
        // Dropping the handle after writing closes git's stdin.
        child.stdin.take().expect("piped stdin").write_all(input)?;
    }
    Ok(child.wait_with_output()?)
}

fn checked(args: &[&str], output: Output) -> Result<Output, CheckoutError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(CheckoutError::Git {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Tree id of the checkout's full working state (tracked changes plus
/// untracked non-ignored files), computed in a throwaway index.
pub fn snapshot_tree(worktree: &Path) -> Result<String, CheckoutError> {
    let index = TempPath::new("trama-index");
    // Managed checkouts are plain clones (`.git` is a directory); only ask
    // Git for the index location otherwise.
    let real_index = if worktree.join(".git").is_dir() {
        worktree.join(".git/index")
    } else {
        worktree.join(git_text(worktree, ["rev-parse", "--git-path", "index"])?)
    };
    if real_index.is_file() {
        // Starting from the real index keeps its stat cache, so unchanged
        // files are not rehashed. Only the copy is modified.
        fs::copy(&real_index, &index.0)?;
    } else {
        let envs = [("GIT_INDEX_FILE", index.0.as_os_str())];
        checked(
            &["read-tree", "HEAD"],
            run_git(worktree, ["read-tree", "HEAD"], &envs, None)?,
        )?;
    }
    let envs = [("GIT_INDEX_FILE", index.0.as_os_str())];
    checked(
        &["add", "-A"],
        run_git(worktree, ["add", "-A", "--", "."], &envs, None)?,
    )?;
    let tree = checked(
        &["write-tree"],
        run_git(worktree, ["write-tree"], &envs, None)?,
    )?;
    Ok(text(tree))
}

/// Commit the working-state snapshot (parent `HEAD`) and point `reference`
/// at it. Branches, the index and the working tree are left untouched.
pub fn record_snapshot(
    worktree: &Path,
    reference: &str,
    message: &str,
) -> Result<String, CheckoutError> {
    let tree = snapshot_tree(worktree)?;
    let envs: Vec<(&str, &OsStr)> = SNAPSHOT_IDENTITY
        .iter()
        .map(|(key, value)| (*key, OsStr::new(*value)))
        .collect();
    let commit = checked(
        &["commit-tree"],
        run_git(
            worktree,
            ["commit-tree", tree.as_str(), "-p", "HEAD", "-m", message],
            &envs,
            None,
        )?,
    )?;
    let commit = text(commit);
    git(worktree, ["update-ref", reference, commit.as_str()])?;
    Ok(commit)
}

/// Resolve `rev` to a commit id, or `None` when it does not exist.
pub fn resolve_commit(worktree: &Path, rev: &str) -> Result<Option<String>, CheckoutError> {
    let spec = format!("{rev}^{{commit}}");
    let output = run_git(
        worktree,
        ["rev-parse", "--verify", "--quiet", &spec],
        &[],
        None,
    )?;
    if output.status.success() {
        Ok(Some(text(output)))
    } else {
        Ok(None)
    }
}

/// Tree id of a commit-ish.
pub fn resolve_tree(worktree: &Path, rev: &str) -> Result<String, CheckoutError> {
    git_text(
        worktree,
        ["rev-parse", "--verify", &format!("{rev}^{{tree}}")],
    )
}

/// Apply a `git diff --binary` patch to the working tree only (no index).
pub fn apply_patch(worktree: &Path, patch: &[u8]) -> Result<(), CheckoutError> {
    let args = ["apply", "--binary", "--whitespace=nowarn", "-"];
    checked(&args, run_git(worktree, args, &[], Some(patch))?)?;
    Ok(())
}

/// Unified diff and per-file stats of the checkout's current working state
/// (including untracked non-ignored files) against `base` (any commit-ish).
pub fn diff(worktree: &Path, base: &str) -> Result<RepoDiff, CheckoutError> {
    let tree = snapshot_tree(worktree)?;
    let range = [base, tree.as_str()];
    let mut patch = git(
        worktree,
        [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            range[0],
            range[1],
        ],
    )?
    .stdout;
    let truncated = patch.len() > MAX_DIFF_BYTES;
    if truncated {
        patch.truncate(MAX_DIFF_BYTES);
    }
    let diff = String::from_utf8_lossy(&patch).into_owned();
    let name_status = git(
        worktree,
        [
            "diff",
            "--no-ext-diff",
            "-M",
            "-z",
            "--name-status",
            range[0],
            range[1],
        ],
    )?
    .stdout;
    let numstat = git(
        worktree,
        [
            "diff",
            "--no-ext-diff",
            "-M",
            "-z",
            "--numstat",
            range[0],
            range[1],
        ],
    )?
    .stdout;
    Ok(RepoDiff {
        diff,
        truncated,
        files: merge_stats(parse_name_status(&name_status), parse_numstat(&numstat)),
    })
}

fn status_from_letter(letter: &str) -> FileChangeStatus {
    match letter.chars().next() {
        Some('A') => FileChangeStatus::Added,
        Some('M') => FileChangeStatus::Modified,
        Some('D') => FileChangeStatus::Deleted,
        Some('R') => FileChangeStatus::Renamed,
        Some('C') => FileChangeStatus::Copied,
        Some('T') => FileChangeStatus::TypeChanged,
        _ => FileChangeStatus::Unknown,
    }
}

fn nul_fields(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

/// `--name-status -z`: `STATUS\0path\0`, or `RNNN\0old\0new\0` for renames
/// and copies.
fn parse_name_status(raw: &[u8]) -> Vec<(FileChangeStatus, Option<String>, String)> {
    let fields = nul_fields(raw);
    let mut out = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = status_from_letter(&fields[index]);
        let two_paths = matches!(status, FileChangeStatus::Renamed | FileChangeStatus::Copied);
        if two_paths && index + 2 < fields.len() {
            out.push((
                status,
                Some(fields[index + 1].clone()),
                fields[index + 2].clone(),
            ));
            index += 3;
        } else if index + 1 < fields.len() {
            out.push((status, None, fields[index + 1].clone()));
            index += 2;
        } else {
            break;
        }
    }
    out
}

/// `--numstat -z`: `ADD\tDEL\tpath\0`, or `ADD\tDEL\t\0old\0new\0` for
/// renames. Binary files report `-` counts.
fn parse_numstat(raw: &[u8]) -> Vec<(Option<u32>, Option<u32>)> {
    let fields: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = String::from_utf8_lossy(fields[index]);
        if field.is_empty() {
            index += 1;
            continue;
        }
        let mut parts = field.splitn(3, '\t');
        let additions = parts.next().and_then(|value| value.parse().ok());
        let deletions = parts.next().and_then(|value| value.parse().ok());
        let path = parts.next().unwrap_or_default();
        out.push((additions, deletions));
        index += if path.is_empty() { 3 } else { 1 };
    }
    out
}

fn merge_stats(
    names: Vec<(FileChangeStatus, Option<String>, String)>,
    counts: Vec<(Option<u32>, Option<u32>)>,
) -> Vec<FileChange> {
    names
        .into_iter()
        .enumerate()
        .map(|(index, (status, old_path, path))| {
            let (additions, deletions) = counts.get(index).copied().unwrap_or((None, None));
            FileChange {
                path,
                old_path,
                status,
                additions,
                deletions,
            }
        })
        .collect()
}

/// One side of a three-way file comparison. `mode` is the Git mode string.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    bytes: Vec<u8>,
}

impl Entry {
    fn is_symlink(&self) -> bool {
        self.mode == "120000"
    }

    fn is_text(&self) -> bool {
        !self.is_symlink() && !self.bytes.contains(&0)
    }
}

/// `path -> (mode, blob id)` for every blob in `tree`.
fn tree_entries(
    worktree: &Path,
    tree: &str,
) -> Result<HashMap<String, (String, String)>, CheckoutError> {
    let raw = git(worktree, ["ls-tree", "-r", "-z", tree])?.stdout;
    let mut out = HashMap::new();
    for record in raw.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let record = String::from_utf8_lossy(record);
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mut meta = meta.split(' ');
        let (Some(mode), Some(kind), Some(id)) = (meta.next(), meta.next(), meta.next()) else {
            continue;
        };
        if kind == "blob" {
            out.insert(path.to_owned(), (mode.to_owned(), id.to_owned()));
        }
    }
    Ok(out)
}

fn blob_entry(
    worktree: &Path,
    entries: &HashMap<String, (String, String)>,
    path: &str,
) -> Result<Option<Entry>, CheckoutError> {
    match entries.get(path) {
        None => Ok(None),
        Some((mode, id)) => Ok(Some(Entry {
            mode: mode.clone(),
            bytes: git(worktree, ["cat-file", "blob", id.as_str()])?.stdout,
        })),
    }
}

/// Containment-checked target inside `root` for a Git-relative path. The
/// parent directory is created when `create_parent` is set.
fn target_path(root: &Path, path: &str, create_parent: bool) -> Result<PathBuf, CheckoutError> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || relative
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".git")
    {
        return Err(CheckoutError::Containment(relative.to_path_buf()));
    }
    let target = root.join(relative);
    let parent = target.parent().expect("joined path has a parent");
    if create_parent {
        fs::create_dir_all(parent)?;
    } else if !parent.exists() {
        return Ok(target);
    }
    let canonical_parent = fs::canonicalize(parent)?;
    if !canonical_parent.starts_with(root) {
        return Err(CheckoutError::Containment(target));
    }
    Ok(target)
}

fn worktree_entry(root: &Path, path: &str) -> Result<Option<Entry>, CheckoutError> {
    let target = target_path(root, path, false)?;
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(&target)?;
        return Ok(Some(Entry {
            mode: "120000".into(),
            bytes: link.as_os_str().as_encoded_bytes().to_vec(),
        }));
    }
    if !metadata.is_file() {
        return Err(CheckoutError::Invalid(target));
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some(Entry {
        mode: if executable { "100755" } else { "100644" }.into(),
        bytes: fs::read(&target)?,
    }))
}

fn write_entry(root: &Path, path: &str, entry: Option<&Entry>) -> Result<(), CheckoutError> {
    let target = target_path(root, path, entry.is_some())?;
    if fs::symlink_metadata(&target).is_ok() {
        fs::remove_file(&target)?;
    }
    let Some(entry) = entry else {
        return Ok(());
    };
    if entry.is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(OsStr::from_bytes(&entry.bytes), &target)?;
        }
        return Ok(());
    }
    fs::write(&target, &entry.bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if entry.mode == "100755" { 0o755 } else { 0o644 };
        fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Three-way text merge with `git merge-file`. Returns the merged bytes and
/// whether conflict markers were produced.
fn merge_text(
    scratch: &Path,
    ours: &[u8],
    base: &[u8],
    theirs: &[u8],
) -> Result<(Vec<u8>, bool), CheckoutError> {
    let files = TempPath::new("trama-merge");
    fs::create_dir_all(&files.0)?;
    let paths = [
        files.0.join("parent"),
        files.0.join("fork-point"),
        files.0.join("subthread"),
    ];
    fs::write(&paths[0], ours)?;
    fs::write(&paths[1], base)?;
    fs::write(&paths[2], theirs)?;
    let output = run_git(
        scratch,
        [
            OsStr::new("merge-file"),
            OsStr::new("-p"),
            OsStr::new("-L"),
            OsStr::new("parent"),
            OsStr::new("-L"),
            OsStr::new("fork point"),
            OsStr::new("-L"),
            OsStr::new("subthread"),
            paths[0].as_os_str(),
            paths[1].as_os_str(),
            paths[2].as_os_str(),
        ],
        &[],
        None,
    )?;
    match output.status.code() {
        Some(0) => Ok((output.stdout, false)),
        Some(code) if (1..=127).contains(&code) => Ok((output.stdout, true)),
        _ => Err(CheckoutError::Git {
            args: vec!["merge-file".into()],
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }),
    }
}

/// Apply what `subthread` changed since `fork_commit` (its current working
/// state, untracked non-ignored files included) to the `parent` checkout's
/// working tree, file by file:
///
/// - the parent still has the fork-point version: take the subthread's;
/// - the parent already has the subthread's version: nothing to do;
/// - both changed text files: three-way merge, writing conflict markers when
///   needed;
/// - otherwise (binary, symlink, delete/modify): report a conflict and leave
///   the parent's file alone.
///
/// Never touches the parent's index, branches or commits, and never modifies
/// the subthread.
pub fn pull(
    subthread: &Path,
    fork_commit: &str,
    parent: &Path,
) -> Result<Vec<PullFileResult>, CheckoutError> {
    let parent = fs::canonicalize(parent)?;
    let current = snapshot_tree(subthread)?;
    let changed = git(
        subthread,
        [
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "-z",
            "--name-status",
            fork_commit,
            current.as_str(),
        ],
    )?
    .stdout;
    let base_entries = tree_entries(subthread, fork_commit)?;
    let current_entries = tree_entries(subthread, &current)?;
    let mut results = Vec::new();
    for (change, _, path) in parse_name_status(&changed) {
        let base = blob_entry(subthread, &base_entries, &path)?;
        let theirs = blob_entry(subthread, &current_entries, &path)?;
        let ours = worktree_entry(&parent, &path)?;
        let outcome = if ours == theirs {
            PullFileOutcome::Unchanged
        } else if ours == base {
            write_entry(&parent, &path, theirs.as_ref())?;
            PullFileOutcome::Applied
        } else {
            match (&ours, &theirs) {
                (Some(_), None) => PullFileOutcome::Conflict {
                    markers_written: false,
                    reason: "changed in the parent but deleted in the subthread".into(),
                },
                (None, Some(_)) => PullFileOutcome::Conflict {
                    markers_written: false,
                    reason: "deleted in the parent but changed in the subthread".into(),
                },
                (Some(ours_entry), Some(theirs_entry))
                    if ours_entry.is_text()
                        && theirs_entry.is_text()
                        && base.as_ref().is_none_or(Entry::is_text) =>
                {
                    let base_bytes = base.as_ref().map(|b| b.bytes.as_slice()).unwrap_or(&[]);
                    let (merged, conflicted) =
                        merge_text(subthread, &ours_entry.bytes, base_bytes, &theirs_entry.bytes)?;
                    // Keep the parent's mode unless only the subthread changed it.
                    let mode = match &base {
                        Some(base) if base.mode == ours_entry.mode => theirs_entry.mode.clone(),
                        _ => ours_entry.mode.clone(),
                    };
                    write_entry(
                        &parent,
                        &path,
                        Some(&Entry {
                            mode,
                            bytes: merged,
                        }),
                    )?;
                    if conflicted {
                        PullFileOutcome::Conflict {
                            markers_written: true,
                            reason: "changed on both sides; conflict markers were written".into(),
                        }
                    } else {
                        PullFileOutcome::Merged
                    }
                }
                _ => PullFileOutcome::Conflict {
                    markers_written: false,
                    reason: "binary file or symlink changed on both sides; the parent's version was kept"
                        .into(),
                },
            }
        };
        results.push(PullFileResult {
            path,
            change,
            outcome,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout::CheckoutManager;
    use trama_domain::ids::{CheckoutId, RepositoryId, ThreadId};

    fn sh_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn porcelain(cwd: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn head(cwd: &Path) -> String {
        git_text(cwd, ["rev-parse", "HEAD"]).unwrap()
    }

    /// Source repo, a parent managed checkout with committed, staged,
    /// unstaged, untracked, ignored, deleted and binary changes.
    struct Fixture {
        _temp: tempfile::TempDir,
        source: PathBuf,
        manager: CheckoutManager,
        parent: PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        sh_git(&source, &["init", "-b", "main"]);
        fs::write(source.join("same.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(source.join("gone.txt"), "delete me\n").unwrap();
        fs::write(source.join("staged.txt"), "before\n").unwrap();
        fs::write(source.join(".gitignore"), "*.secret\n").unwrap();
        sh_git(&source, &["add", "."]);
        sh_git(&source, &["commit", "-m", "initial"]);
        let manager = CheckoutManager::new(temp.path().join("managed"));
        let parent = manager
            .provision_repo_local(
                &source,
                &ThreadId::from("parent"),
                &RepositoryId::from("repo"),
                &CheckoutId::from("parent-checkout"),
            )
            .unwrap()
            .path;
        // A commit the parent made that the source does not have.
        fs::write(parent.join("committed.txt"), "parent commit\n").unwrap();
        sh_git(&parent, &["add", "committed.txt"]);
        sh_git(&parent, &["commit", "-m", "parent work"]);
        // Uncommitted state.
        fs::write(parent.join("same.txt"), "one\ntwo changed\nthree\n").unwrap();
        fs::write(parent.join("staged.txt"), "after\n").unwrap();
        sh_git(&parent, &["add", "staged.txt"]);
        fs::remove_file(parent.join("gone.txt")).unwrap();
        fs::create_dir_all(parent.join("new/dir")).unwrap();
        fs::write(parent.join("new/dir/untracked.txt"), "untracked\n").unwrap();
        fs::write(parent.join("blob.bin"), [0u8, 1, 2, 255, 0, 7]).unwrap();
        fs::write(parent.join("token.secret"), "ignored\n").unwrap();
        Fixture {
            _temp: temp,
            source,
            manager,
            parent,
        }
    }

    fn copy(fixture: &Fixture, thread: &str) -> crate::checkout::ProvisionedCopy {
        fixture
            .manager
            .provision_copy(
                &fixture.parent,
                &fixture.source,
                &ThreadId::from(thread),
                &RepositoryId::from("repo"),
                &CheckoutId::from(format!("{thread}-checkout")),
            )
            .unwrap()
    }

    #[test]
    fn copy_contains_uncommitted_and_untracked_changes_and_leaves_parent_untouched() {
        let fixture = fixture();
        let parent_status = porcelain(&fixture.parent);
        let parent_head = head(&fixture.parent);
        let source_head = head(&fixture.source);

        let copy = copy(&fixture, "sub");
        assert_eq!(copy.base_revision, parent_head);
        assert_eq!(head(&copy.path), parent_head);
        assert_eq!(
            fs::read_to_string(copy.path.join("same.txt")).unwrap(),
            "one\ntwo changed\nthree\n"
        );
        assert_eq!(
            fs::read_to_string(copy.path.join("staged.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(copy.path.join("committed.txt")).unwrap(),
            "parent commit\n"
        );
        assert_eq!(
            fs::read_to_string(copy.path.join("new/dir/untracked.txt")).unwrap(),
            "untracked\n"
        );
        assert_eq!(
            fs::read(copy.path.join("blob.bin")).unwrap(),
            vec![0u8, 1, 2, 255, 0, 7]
        );
        assert!(!copy.path.join("gone.txt").exists());
        assert!(!copy.path.join("token.secret").exists());
        assert_eq!(
            resolve_commit(&copy.path, FORK_POINT_REF)
                .unwrap()
                .as_deref(),
            Some(copy.fork_commit.as_str())
        );
        // The copy is a valid repo-local checkout of the original source.
        assert_eq!(
            copy.path,
            fs::canonicalize(&fixture.source)
                .unwrap()
                .join(".trama/worktrees/sub/sub-checkout")
        );
        fixture
            .manager
            .validate_repo_local(&fixture.source, &copy.path, &copy.base_revision)
            .unwrap();

        // Parent and source are untouched (index included: staged stays staged).
        assert_eq!(porcelain(&fixture.parent), parent_status);
        assert_eq!(head(&fixture.parent), parent_head);
        assert!(fixture.parent.join("token.secret").exists());
        assert_eq!(head(&fixture.source), source_head);
        assert!(porcelain(&fixture.source).is_empty());

        // Diverges: edits in the copy never reach the parent.
        fs::write(copy.path.join("same.txt"), "copy edit\n").unwrap();
        assert_eq!(
            fs::read_to_string(fixture.parent.join("same.txt")).unwrap(),
            "one\ntwo changed\nthree\n"
        );

        // Re-provisioning returns the existing copy without replacing it.
        let again = copy_again(&fixture);
        assert_eq!(again.fork_commit, copy.fork_commit);
        assert_eq!(again.base_revision, copy.base_revision);
        assert_eq!(
            fs::read_to_string(copy.path.join("same.txt")).unwrap(),
            "copy edit\n"
        );
    }

    fn copy_again(fixture: &Fixture) -> crate::checkout::ProvisionedCopy {
        copy(fixture, "sub")
    }

    #[test]
    fn copy_refuses_the_primary_repository_as_parent() {
        let fixture = fixture();
        fs::write(fixture.source.join("dirty.txt"), "primary work\n").unwrap();
        let result = fixture.manager.provision_copy(
            &fixture.source,
            &fixture.source,
            &ThreadId::from("sub"),
            &RepositoryId::from("repo"),
            &CheckoutId::from("sub-checkout"),
        );
        assert!(matches!(result, Err(CheckoutError::Invalid(_))));
        assert!(
            !fixture
                .source
                .join(".trama/worktrees/sub")
                .join("sub-checkout")
                .exists()
        );
    }

    #[test]
    fn copy_of_detached_parent_keeps_its_head() {
        let fixture = fixture();
        sh_git(&fixture.parent, &["stash", "--include-untracked"]);
        sh_git(
            &fixture.parent,
            &["checkout", "--quiet", "--detach", "HEAD"],
        );
        fs::write(fixture.parent.join("detached.txt"), "x\n").unwrap();
        sh_git(&fixture.parent, &["add", "detached.txt"]);
        sh_git(&fixture.parent, &["commit", "-m", "detached commit"]);
        let copy = copy(&fixture, "detached");
        assert_eq!(head(&copy.path), head(&fixture.parent));
        assert!(copy.path.join("detached.txt").exists());
    }

    #[test]
    fn diff_reports_text_stats_untracked_binary_and_renames() {
        let fixture = fixture();
        let base = head(&fixture.parent);
        fs::rename(
            fixture.parent.join("committed.txt"),
            fixture.parent.join("renamed.txt"),
        )
        .unwrap();
        let changes = diff(&fixture.parent, &base).unwrap();
        assert!(!changes.truncated);
        assert!(
            changes.diff.contains("-two\n+two changed"),
            "{}",
            changes.diff
        );
        assert!(changes.diff.contains("new/dir/untracked.txt"));
        assert!(changes.diff.contains("Binary files"));
        assert!(!changes.diff.contains("token.secret"));
        assert!(!changes.diff.contains("\x1b["));
        let by_path: HashMap<_, _> = changes.files.iter().map(|f| (f.path.as_str(), f)).collect();
        let same = by_path["same.txt"];
        assert_eq!(same.status, FileChangeStatus::Modified);
        assert_eq!((same.additions, same.deletions), (Some(1), Some(1)));
        assert_eq!(by_path["gone.txt"].status, FileChangeStatus::Deleted);
        assert_eq!(
            by_path["new/dir/untracked.txt"].status,
            FileChangeStatus::Added
        );
        let binary = by_path["blob.bin"];
        assert_eq!((binary.additions, binary.deletions), (None, None));
        let renamed = by_path["renamed.txt"];
        assert_eq!(renamed.status, FileChangeStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("committed.txt"));
        assert!(!by_path.contains_key("token.secret"));

        // HEAD-relative diff of a clean tree is empty.
        let clean = diff(&fixture.source, "HEAD").unwrap();
        assert!(clean.files.is_empty() && clean.diff.is_empty());
    }

    #[test]
    fn last_turn_snapshot_bases_diff_on_turn_start() {
        let fixture = fixture();
        let snapshot = record_snapshot(&fixture.parent, LAST_TURN_REF, "turn").unwrap();
        assert_eq!(
            resolve_commit(&fixture.parent, LAST_TURN_REF).unwrap(),
            Some(snapshot)
        );
        let status = porcelain(&fixture.parent);
        fs::write(fixture.parent.join("turn.txt"), "during turn\n").unwrap();
        let changes = diff(&fixture.parent, LAST_TURN_REF).unwrap();
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, "turn.txt");
        fs::remove_file(fixture.parent.join("turn.txt")).unwrap();
        assert_eq!(porcelain(&fixture.parent), status);
        assert_eq!(
            resolve_commit(&fixture.parent, "refs/trama/missing").unwrap(),
            None
        );
    }

    #[test]
    fn pull_applies_merges_and_reports_conflicts_without_committing() {
        let fixture = fixture();
        let copy = copy(&fixture, "sub");
        let parent_head = head(&fixture.parent);
        // Subthread edits.
        fs::write(copy.path.join("staged.txt"), "after\nfrom subthread\n").unwrap();
        fs::write(
            copy.path.join("same.txt"),
            "one\ntwo changed\nthree\nfour\n",
        )
        .unwrap();
        fs::write(copy.path.join("fresh.txt"), "new in subthread\n").unwrap();
        fs::write(copy.path.join("conflict.txt"), "subthread side\n").unwrap();
        fs::remove_file(copy.path.join("new/dir/untracked.txt")).unwrap();
        fs::write(copy.path.join("blob.bin"), [9u8, 0, 9]).unwrap();
        fs::write(copy.path.join("ignored.secret"), "never pulled\n").unwrap();
        // Parent moved on meanwhile.
        fs::write(
            fixture.parent.join("same.txt"),
            "zero\none\ntwo changed\nthree\n",
        )
        .unwrap();
        fs::write(fixture.parent.join("conflict.txt"), "parent side\n").unwrap();
        fs::write(fixture.parent.join("blob.bin"), [7u8, 0, 7]).unwrap();

        let results = pull(&copy.path, &copy.fork_commit, &fixture.parent).unwrap();
        let by_path: HashMap<_, _> = results.iter().map(|r| (r.path.as_str(), r)).collect();
        assert_eq!(by_path["staged.txt"].outcome, PullFileOutcome::Applied);
        assert_eq!(by_path["staged.txt"].change, FileChangeStatus::Modified);
        assert_eq!(by_path["same.txt"].outcome, PullFileOutcome::Merged);
        assert_eq!(by_path["fresh.txt"].outcome, PullFileOutcome::Applied);
        assert_eq!(by_path["fresh.txt"].change, FileChangeStatus::Added);
        assert_eq!(
            by_path["new/dir/untracked.txt"].outcome,
            PullFileOutcome::Applied
        );
        assert!(matches!(
            by_path["conflict.txt"].outcome,
            PullFileOutcome::Conflict {
                markers_written: true,
                ..
            }
        ));
        assert!(matches!(
            by_path["blob.bin"].outcome,
            PullFileOutcome::Conflict {
                markers_written: false,
                ..
            }
        ));
        assert!(!by_path.contains_key("ignored.secret"));

        let read = |path: &str| fs::read_to_string(fixture.parent.join(path)).unwrap();
        assert_eq!(read("staged.txt"), "after\nfrom subthread\n");
        assert_eq!(read("same.txt"), "zero\none\ntwo changed\nthree\nfour\n");
        assert_eq!(read("fresh.txt"), "new in subthread\n");
        assert!(!fixture.parent.join("new/dir/untracked.txt").exists());
        let conflict = read("conflict.txt");
        assert!(conflict.contains("<<<<<<< parent"), "{conflict}");
        assert!(conflict.contains("subthread side"));
        assert_eq!(
            fs::read(fixture.parent.join("blob.bin")).unwrap(),
            vec![7u8, 0, 7]
        );
        assert!(!fixture.parent.join("ignored.secret").exists());
        // Never committed.
        assert_eq!(head(&fixture.parent), parent_head);

        // Pulling again is idempotent for already-applied files.
        let again = pull(&copy.path, &copy.fork_commit, &fixture.parent).unwrap();
        let again: HashMap<_, _> = again.iter().map(|r| (r.path.as_str(), r)).collect();
        assert_eq!(again["fresh.txt"].outcome, PullFileOutcome::Unchanged);
        assert_eq!(again["staged.txt"].outcome, PullFileOutcome::Unchanged);
    }

    #[test]
    fn numstat_and_name_status_parsers_handle_renames() {
        let names = parse_name_status(b"M\0a.txt\0R090\0old.txt\0new.txt\0D\0gone\0");
        assert_eq!(names.len(), 3);
        assert_eq!(names[1].0, FileChangeStatus::Renamed);
        assert_eq!(names[1].1.as_deref(), Some("old.txt"));
        assert_eq!(names[1].2, "new.txt");
        let counts = parse_numstat(b"1\t2\ta.txt\x003\t0\t\0old.txt\0new.txt\0-\t-\tgone\0");
        assert_eq!(
            counts,
            vec![(Some(1), Some(2)), (Some(3), Some(0)), (None, None)]
        );
    }
}
