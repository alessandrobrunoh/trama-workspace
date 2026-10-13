//! Exact checkout snapshots for sharing (plan `docs/plan/04`).
//!
//! A shareable state is `HEAD` + the staged tree of the real index + the
//! working tree (tracked changes and untracked non-ignored files). Capture
//! writes two synthetic commits (both with parent `HEAD`) and keeps them
//! reachable from refs under `refs/trama/share/<share>/<state>/{index,working}`,
//! so a bundle of those refs always carries *both* trees.
//!
//! Capture never mutates `HEAD`, branches, the real index or the working tree
//! (it only adds objects and `refs/trama/share/*` refs to the checkout).
//! Restore only ever runs in a caller-provided, clean **staging** clone that
//! is not exposed as an operational checkout; it runs no hooks and verifies
//! every object id against the expected state before reporting success.
//! Encryption, upload and secret scanning are separate layers (plan 06).

use crate::changes::{SNAPSHOT_IDENTITY, snapshot_tree};
use crate::checkout::{CheckoutError, git, git_text, text};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use trama_domain::checkout_state::{HeadRef, ObjectFormat};

/// Prefix of all share snapshot refs. Never pushed to `origin`.
pub const SHARE_REF_PREFIX: &str = "refs/trama/share/";

/// Errors specific to share snapshots, on top of plain Git failures.
#[derive(Debug, thiserror::Error)]
pub enum ShareGitError {
    #[error(transparent)]
    Checkout(#[from] CheckoutError),
    #[error(
        "the index has unmerged entries; resolve the conflict before sharing. an exact state with conflict stages is not supported yet"
    )]
    UnmergedIndex,
    #[error("invalid share ref component {0:?}")]
    InvalidComponent(String),
    #[error("staging checkout at {0} is not clean; restore needs a fresh staging clone")]
    StagingDirty(PathBuf),
    #[error("bundle carries unexpected ref {0:?}")]
    UnexpectedRef(String),
    #[error("object format mismatch: expected {expected:?}, staging uses {actual:?}")]
    ObjectFormatMismatch {
        expected: ObjectFormat,
        actual: Option<ObjectFormat>,
    },
    #[error("restored {what} is {actual}, expected {expected}")]
    Mismatch {
        what: &'static str,
        expected: String,
        actual: String,
    },
}

pub type Result<T> = std::result::Result<T, ShareGitError>;

/// What capture recorded for one `(thread, repository)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedState {
    pub object_format: ObjectFormat,
    pub head_commit: String,
    pub head_ref: HeadRef,
    pub index_tree: String,
    pub working_tree: String,
    /// Synthetic commit `index_tree` with parent `head_commit`.
    pub index_commit: String,
    /// Synthetic commit `working_tree` with parent `head_commit`.
    pub working_commit: String,
    pub index_ref: String,
    pub working_ref: String,
}

/// Refs naming one captured state.
pub fn share_refs(share: &str, state: &str) -> Result<(String, String)> {
    for component in [share, state] {
        let ok = !component.is_empty()
            && component.len() <= 128
            && component
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
        if !ok {
            return Err(ShareGitError::InvalidComponent(component.to_owned()));
        }
    }
    let base = format!("{SHARE_REF_PREFIX}{share}/{state}");
    Ok((format!("{base}/index"), format!("{base}/working")))
}

/// Object format of a repository (`sha1` when unset).
pub fn object_format(worktree: &Path) -> Result<ObjectFormat> {
    let raw = git_text(worktree, ["rev-parse", "--show-object-format"])?;
    ObjectFormat::parse(&raw).ok_or(ShareGitError::ObjectFormatMismatch {
        expected: ObjectFormat::Sha1,
        actual: None,
    })
}

/// Run git with fixed snapshot identity and optional private index.
fn git_env(cwd: &Path, args: &[&str], index: Option<&Path>) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in SNAPSHOT_IDENTITY {
        command.env(key, value);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().map_err(CheckoutError::from)?;
    if !output.status.success() {
        return Err(CheckoutError::Git {
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }
        .into());
    }
    Ok(text(output))
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn index_path(worktree: &Path) -> Result<PathBuf> {
    let path = git_text(worktree, ["rev-parse", "--git-path", "index"])?;
    let path = PathBuf::from(path);
    Ok(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    })
}

/// Tree of the real index, computed on a private copy so the real index
/// file (including its cache-tree extension) is never rewritten.
pub fn staged_tree(worktree: &Path) -> Result<String> {
    if !git_text(worktree, ["ls-files", "--unmerged"])?.is_empty() {
        return Err(ShareGitError::UnmergedIndex);
    }
    let real = index_path(worktree)?;
    let copy =
        TempFile(std::env::temp_dir().join(format!("trama-share-index-{}", ulid::Ulid::new())));
    if real.is_file() {
        fs::copy(&real, &copy.0).map_err(CheckoutError::from)?;
    } else {
        git_env(worktree, &["read-tree", "HEAD"], Some(&copy.0))?;
    }
    git_env(worktree, &["write-tree"], Some(&copy.0))
}

fn head_ref(worktree: &Path) -> Result<HeadRef> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .map_err(CheckoutError::from)?;
    let name = text(output.clone());
    Ok(if output.status.success() && !name.is_empty() {
        HeadRef::Branch { name }
    } else {
        HeadRef::Detached
    })
}

/// Capture the exact shareable state of a managed checkout. The caller must
/// hold the checkout reservation (no agent/fork/pull writing meanwhile);
/// capture retries once if `HEAD` or the index moved underneath it.
pub fn capture(worktree: &Path, share: &str, state: &str) -> Result<CapturedState> {
    let (index_ref, working_ref) = share_refs(share, state)?;
    let format = object_format(worktree)?;
    let mut attempt = 0;
    let (head_commit, head, index_tree, working_tree) = loop {
        let head_commit = git_text(worktree, ["rev-parse", "--verify", "HEAD^{commit}"])?;
        let head = head_ref(worktree)?;
        let index_tree = staged_tree(worktree)?;
        let working_tree = snapshot_tree(worktree)?;
        let stable = git_text(worktree, ["rev-parse", "--verify", "HEAD^{commit}"])? == head_commit
            && staged_tree(worktree)? == index_tree;
        if stable {
            break (head_commit, head, index_tree, working_tree);
        }
        attempt += 1;
        if attempt >= 2 {
            return Err(CheckoutError::Git {
                args: vec!["capture".into()],
                stderr: "checkout changed during capture".into(),
            }
            .into());
        }
    };
    let commit = |tree: &str, message: &str| {
        git_env(
            worktree,
            &["commit-tree", tree, "-p", &head_commit, "-m", message],
            None,
        )
    };
    let index_commit = commit(&index_tree, "trama: share snapshot (index)")?;
    let working_commit = commit(&working_tree, "trama: share snapshot (working tree)")?;
    git(
        worktree,
        ["update-ref", index_ref.as_str(), index_commit.as_str()],
    )?;
    git(
        worktree,
        ["update-ref", working_ref.as_str(), working_commit.as_str()],
    )?;
    Ok(CapturedState {
        object_format: format,
        head_commit,
        head_ref: head,
        index_tree,
        working_tree,
        index_commit,
        working_commit,
        index_ref,
        working_ref,
    })
}

/// Commits in `head..` that are not reachable from any of `known` (e.g.
/// remote-tracking refs the recipient can fetch from origin). These are the
/// private commits that must travel in the bundle.
pub fn unpublished_commits(worktree: &Path, head: &str, known: &[&str]) -> Result<Vec<String>> {
    let mut args = vec!["rev-list".to_owned(), head.to_owned()];
    for rev in known {
        args.push(format!("^{rev}"));
    }
    let out = git_text(worktree, &args)?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// Write a Git bundle containing both share refs and only the objects not
/// reachable from `prerequisites` (commits the recipient gets from origin).
pub fn export_bundle(
    worktree: &Path,
    captured: &CapturedState,
    prerequisites: &[&str],
    bundle: &Path,
) -> Result<()> {
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("bundle"),
        OsStr::new("create"),
        OsStr::new("--quiet"),
        bundle.as_os_str(),
        OsStr::new(&captured.index_ref),
        OsStr::new(&captured.working_ref),
    ];
    let negated: Vec<String> = prerequisites.iter().map(|rev| format!("^{rev}")).collect();
    args.extend(negated.iter().map(OsStr::new));
    git(worktree, args)?;
    Ok(())
}

/// The state a restore must reproduce exactly (from a verified manifest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedState {
    pub object_format: ObjectFormat,
    pub head_commit: String,
    pub head_ref: HeadRef,
    pub index_tree: String,
    pub working_tree: String,
    pub index_ref: String,
    pub working_ref: String,
}

impl From<&CapturedState> for ExpectedState {
    fn from(c: &CapturedState) -> Self {
        Self {
            object_format: c.object_format,
            head_commit: c.head_commit.clone(),
            head_ref: c.head_ref.clone(),
            index_tree: c.index_tree.clone(),
            working_tree: c.working_tree.clone(),
            index_ref: c.index_ref.clone(),
            working_ref: c.working_ref.clone(),
        }
    }
}

/// Empty directory used as `core.hooksPath` so no hook can run. A real
/// directory (instead of `/dev/null`) behaves the same on every platform.
fn empty_hooks_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("trama-share-no-hooks");
    fs::create_dir_all(&dir).map_err(CheckoutError::from)?;
    Ok(dir)
}

/// Git in a staging clone: no hooks, no fsmonitor, no system config.
fn staging_git(staging: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    use std::io::Write;
    let hooks = format!("core.hooksPath={}", empty_hooks_dir()?.display());
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(staging)
        // Byte-exact materialization: no line-ending conversion on checkout.
        .args([
            "-c",
            hooks.as_str(),
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(CheckoutError::from)?;
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input)
            .map_err(CheckoutError::from)?;
    }
    let output = child.wait_with_output().map_err(CheckoutError::from)?;
    if !output.status.success() {
        return Err(CheckoutError::Git {
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }
        .into());
    }
    Ok(output.stdout)
}

fn staging_text(staging: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&staging_git(staging, args, None)?)
        .trim()
        .to_owned())
}

fn expect(what: &'static str, expected: &str, actual: String) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(ShareGitError::Mismatch {
            what,
            expected: expected.to_owned(),
            actual,
        })
    }
}

/// Materialize `expected` in a clean staging clone from `bundle`.
///
/// The staging clone must already hold the bundle's prerequisites (cloned
/// or fetched from origin with the recipient's own credentials).
/// Sequence: verify bundle → fetch share refs → point `HEAD` →
/// `read-tree --reset -u <index_tree>` → apply `index..working` binary diff
/// to the working tree only → verify `HEAD`, staged and working trees.
pub fn restore(staging: &Path, bundle: &Path, expected: &ExpectedState) -> Result<()> {
    let bundle_str = bundle
        .to_str()
        .ok_or_else(|| CheckoutError::Invalid(bundle.to_path_buf()))?;
    let actual_format = object_format(staging).ok();
    if actual_format != Some(expected.object_format) {
        return Err(ShareGitError::ObjectFormatMismatch {
            expected: expected.object_format,
            actual: actual_format,
        });
    }
    if !staging_text(
        staging,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=no",
        ],
    )?
    .is_empty()
    {
        return Err(ShareGitError::StagingDirty(staging.to_path_buf()));
    }
    for rev in [
        &expected.head_commit,
        &expected.index_tree,
        &expected.working_tree,
    ] {
        if !expected.object_format.is_valid_oid(rev) {
            return Err(ShareGitError::InvalidComponent(rev.clone()));
        }
    }

    // Prerequisites present and refs limited to what the manifest names.
    staging_git(staging, &["bundle", "verify", "--quiet", bundle_str], None)?;
    let heads = staging_text(staging, &["bundle", "list-heads", bundle_str])?;
    for line in heads.lines() {
        let name = line.split_whitespace().nth(1).unwrap_or_default();
        if name != expected.index_ref && name != expected.working_ref {
            return Err(ShareGitError::UnexpectedRef(name.to_owned()));
        }
    }
    let index_spec = format!("{0}:{0}", expected.index_ref);
    let working_spec = format!("{0}:{0}", expected.working_ref);
    staging_git(
        staging,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            bundle_str,
            &index_spec,
            &working_spec,
        ],
        None,
    )?;
    let parent = |r: &str| staging_text(staging, &["rev-parse", "--verify", &format!("{r}^1")]);
    let tree = |r: &str| {
        staging_text(
            staging,
            &["rev-parse", "--verify", &format!("{r}^{{tree}}")],
        )
    };
    expect(
        "index snapshot parent",
        &expected.head_commit,
        parent(&expected.index_ref)?,
    )?;
    expect(
        "working snapshot parent",
        &expected.head_commit,
        parent(&expected.working_ref)?,
    )?;
    expect(
        "index snapshot tree",
        &expected.index_tree,
        tree(&expected.index_ref)?,
    )?;
    expect(
        "working snapshot tree",
        &expected.working_tree,
        tree(&expected.working_ref)?,
    )?;

    // HEAD without `checkout` (no post-checkout hook, no implicit tracking).
    match &expected.head_ref {
        HeadRef::Branch { name } => {
            let full = format!("refs/heads/{name}");
            staging_git(staging, &["check-ref-format", &full], None)?;
            staging_git(staging, &["update-ref", &full, &expected.head_commit], None)?;
            staging_git(staging, &["symbolic-ref", "HEAD", &full], None)?;
        }
        HeadRef::Detached => {
            staging_git(
                staging,
                &["update-ref", "--no-deref", "HEAD", &expected.head_commit],
                None,
            )?;
        }
    }
    staging_git(
        staging,
        &["read-tree", "--reset", "-u", &expected.index_tree],
        None,
    )?;
    let patch = staging_git(
        staging,
        &[
            "diff",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            &expected.index_tree,
            &expected.working_tree,
        ],
        None,
    )?;
    if !patch.is_empty() {
        staging_git(
            staging,
            &["apply", "--binary", "--whitespace=nowarn", "-"],
            Some(&patch),
        )?;
    }

    expect(
        "HEAD",
        &expected.head_commit,
        staging_text(staging, &["rev-parse", "HEAD"])?,
    )?;
    expect("staged tree", &expected.index_tree, staged_tree(staging)?)?;
    expect(
        "working tree",
        &expected.working_tree,
        snapshot_tree(staging)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(cwd: &Path, args: &[&str]) {
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

    fn out(cwd: &Path, args: &[&str]) -> Vec<u8> {
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap()
            .stdout
    }

    fn status_z(cwd: &Path) -> Vec<u8> {
        out(
            cwd,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
    }

    struct Fixture {
        temp: tempfile::TempDir,
        origin: PathBuf,
        alice: PathBuf,
    }

    /// Origin (bare) with one pushed commit; Alice's clone with a private
    /// commit, staged add, staged+unstaged edit of one file, staged rename,
    /// staged delete, unstaged delete, untracked file, executable bit,
    /// symlink, binary with NUL bytes and an ignored `.env`.
    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let seed = temp.path().join("seed");
        fs::create_dir_all(&seed).unwrap();
        sh(&seed, &["init", "-q", "-b", "main"]);
        fs::write(seed.join("same.txt"), "one\n").unwrap();
        fs::write(seed.join("rename-me.txt"), "rename\n").unwrap();
        fs::write(seed.join("delete-staged.txt"), "x\n").unwrap();
        fs::write(seed.join("delete-unstaged.txt"), "y\n").unwrap();
        fs::write(seed.join(".gitignore"), ".env\n").unwrap();
        sh(&seed, &["add", "-A"]);
        sh(&seed, &["commit", "-q", "-m", "initial"]);
        let origin = temp.path().join("origin.git");
        sh(
            temp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        let alice = temp.path().join("alice");
        sh(
            temp.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                alice.to_str().unwrap(),
            ],
        );

        fs::write(alice.join("private.txt"), "local commit\n").unwrap();
        sh(&alice, &["add", "private.txt"]);
        sh(&alice, &["commit", "-q", "-m", "private"]);

        fs::write(alice.join("staged-add.txt"), "A\n").unwrap();
        fs::write(alice.join("same.txt"), "two\n").unwrap();
        sh(&alice, &["add", "staged-add.txt", "same.txt"]);
        fs::write(alice.join("same.txt"), "three\n").unwrap();
        sh(&alice, &["mv", "rename-me.txt", "renamed.txt"]);
        sh(&alice, &["rm", "-q", "delete-staged.txt"]);
        fs::remove_file(alice.join("delete-unstaged.txt")).unwrap();
        fs::write(alice.join("untracked.txt"), "C\n").unwrap();
        fs::write(alice.join("blob.bin"), [0u8, 1, 2, 0, 255]).unwrap();
        fs::write(alice.join(".env"), "SECRET=1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(alice.join("run.sh"), "#!/bin/sh\n").unwrap();
            fs::set_permissions(alice.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("same.txt", alice.join("link")).unwrap();
        }
        Fixture {
            temp,
            origin,
            alice,
        }
    }

    fn fingerprint(repo: &Path) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            fs::read(repo.join(".git/index")).unwrap(),
            out(repo, &["rev-parse", "HEAD"]),
            out(repo, &["for-each-ref", "refs/heads", "refs/remotes"]),
            status_z(repo),
        )
    }

    #[test]
    fn capture_export_restore_roundtrip_is_exact() {
        let fx = fixture();
        let before = fingerprint(&fx.alice);
        let captured = capture(&fx.alice, "share1", "state0").unwrap();
        assert_eq!(
            fingerprint(&fx.alice),
            before,
            "capture must not mutate the source"
        );
        assert_ne!(captured.index_tree, captured.working_tree);
        assert_eq!(
            captured.head_ref,
            HeadRef::Branch {
                name: "main".into()
            }
        );

        let private =
            unpublished_commits(&fx.alice, &captured.head_commit, &["origin/main"]).unwrap();
        assert_eq!(private.len(), 1);
        let bundle = fx.temp.path().join("state.bundle");
        export_bundle(&fx.alice, &captured, &["origin/main"], &bundle).unwrap();

        // Bob clones origin with his own access, then restores in staging.
        let bob = fx.temp.path().join("bob");
        sh(
            fx.temp.path(),
            &[
                "clone",
                "-q",
                fx.origin.to_str().unwrap(),
                bob.to_str().unwrap(),
            ],
        );
        restore(&bob, &bundle, &ExpectedState::from(&captured)).unwrap();

        assert_eq!(status_z(&bob), status_z(&fx.alice));
        assert_eq!(
            out(&bob, &["rev-parse", "HEAD"]),
            out(&fx.alice, &["rev-parse", "HEAD"])
        );
        assert_eq!(fs::read(bob.join("same.txt")).unwrap(), b"three\n");
        assert_eq!(out(&bob, &["show", ":same.txt"]), b"two\n");
        assert_eq!(fs::read(bob.join("blob.bin")).unwrap(), [0u8, 1, 2, 0, 255]);
        assert!(!bob.join(".env").exists(), "ignored files are never shared");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(bob.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
            assert!(
                fs::symlink_metadata(bob.join("link"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert_eq!(
            fingerprint(&fx.alice),
            before,
            "export must not mutate the source"
        );
    }

    #[test]
    fn detached_head_and_clean_state() {
        let fx = fixture();
        sh(&fx.alice, &["stash", "-q", "-u"]);
        sh(&fx.alice, &["checkout", "-q", "--detach"]);
        let captured = capture(&fx.alice, "s", "detached").unwrap();
        assert_eq!(captured.head_ref, HeadRef::Detached);
        assert_eq!(captured.index_tree, captured.working_tree);
        let bundle = fx.temp.path().join("d.bundle");
        export_bundle(&fx.alice, &captured, &["origin/main"], &bundle).unwrap();
        let bob = fx.temp.path().join("bob");
        sh(
            fx.temp.path(),
            &[
                "clone",
                "-q",
                fx.origin.to_str().unwrap(),
                bob.to_str().unwrap(),
            ],
        );
        restore(&bob, &bundle, &ExpectedState::from(&captured)).unwrap();
        assert!(status_z(&bob).is_empty());
        assert!(out(&bob, &["symbolic-ref", "-q", "HEAD"]).is_empty());
    }

    #[test]
    fn missing_prerequisite_fails_before_touching_staging() {
        let fx = fixture();
        let captured = capture(&fx.alice, "s", "st").unwrap();
        let bundle = fx.temp.path().join("p.bundle");
        // Pretend the private commit is on origin: the bundle then needs it.
        export_bundle(&fx.alice, &captured, &["HEAD"], &bundle).unwrap();
        let bob = fx.temp.path().join("bob");
        sh(
            fx.temp.path(),
            &[
                "clone",
                "-q",
                fx.origin.to_str().unwrap(),
                bob.to_str().unwrap(),
            ],
        );
        let head_before = out(&bob, &["rev-parse", "HEAD"]);
        assert!(restore(&bob, &bundle, &ExpectedState::from(&captured)).is_err());
        assert_eq!(out(&bob, &["rev-parse", "HEAD"]), head_before);
        assert!(status_z(&bob).is_empty());
    }

    #[test]
    fn tampered_expectation_and_dirty_staging_are_rejected() {
        let fx = fixture();
        let captured = capture(&fx.alice, "s", "st").unwrap();
        let bundle = fx.temp.path().join("t.bundle");
        export_bundle(&fx.alice, &captured, &["origin/main"], &bundle).unwrap();
        let bob = fx.temp.path().join("bob");
        sh(
            fx.temp.path(),
            &[
                "clone",
                "-q",
                fx.origin.to_str().unwrap(),
                bob.to_str().unwrap(),
            ],
        );

        let mut wrong = ExpectedState::from(&captured);
        wrong.working_tree = captured.index_tree.clone();
        assert!(matches!(
            restore(&bob, &bundle, &wrong),
            Err(ShareGitError::Mismatch { .. })
        ));
        let mut wrong_ref = ExpectedState::from(&captured);
        wrong_ref.index_ref = "refs/trama/share/s/other/index".into();
        assert!(matches!(
            restore(&bob, &bundle, &wrong_ref),
            Err(ShareGitError::UnexpectedRef(_))
        ));

        fs::write(bob.join("local.txt"), "bob's work\n").unwrap();
        assert!(matches!(
            restore(&bob, &bundle, &ExpectedState::from(&captured)),
            Err(ShareGitError::StagingDirty(_))
        ));
        assert_eq!(fs::read(bob.join("local.txt")).unwrap(), b"bob's work\n");
    }

    #[test]
    fn unmerged_index_blocks_capture() {
        let fx = fixture();
        let repo = &fx.alice;
        sh(repo, &["stash", "-q", "-u"]);
        sh(repo, &["checkout", "-q", "-b", "other"]);
        fs::write(repo.join("same.txt"), "other\n").unwrap();
        sh(repo, &["commit", "-q", "-am", "other"]);
        sh(repo, &["checkout", "-q", "main"]);
        fs::write(repo.join("same.txt"), "main\n").unwrap();
        sh(repo, &["commit", "-q", "-am", "main"]);
        let merge = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["merge", "-q", "other"])
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(!merge.status.success(), "merge must conflict");
        let result = capture(repo, "s", "st");
        assert!(
            matches!(result, Err(ShareGitError::UnmergedIndex)),
            "{result:?}"
        );
    }

    #[test]
    fn ref_components_are_validated() {
        assert!(share_refs("ok-1", "st_2").is_ok());
        for bad in ["", "a/b", "..", "a b", "x:y"] {
            assert!(share_refs(bad, "s").is_err(), "{bad:?}");
            assert!(share_refs("s", bad).is_err(), "{bad:?}");
        }
    }
}
