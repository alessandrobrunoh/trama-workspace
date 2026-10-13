//! Per-thread managed Git clones.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use trama_domain::ids::{RepositoryId, ThreadId};

#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    #[error("checkout I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("git {args:?} failed: {stderr}")]
    Git { args: Vec<String>, stderr: String },
    #[error(
        "repository has uncommitted or untracked non-ignored files at {path}; commit or stash them before cloning. those changes stay in the original working tree and are not copied or deleted"
    )]
    Dirty { path: PathBuf },
    #[error(
        "path does not exist: {path}. choose an existing local Git working tree, not a remote URL"
    )]
    Missing { path: PathBuf },
    #[error(
        "{path} is not the top-level working tree of a non-bare Git repository. select the directory that contains .git, not a subdirectory, a parent folder, or a bare repo"
    )]
    NotWorktree { path: PathBuf },
    #[error("{path} is a bare Git repository. select the non-bare working tree instead")]
    Bare { path: PathBuf },
    #[error("{path} has no commits yet. make an initial commit before adding it")]
    NoCommit { path: PathBuf },
    #[error("invalid managed checkout at {0}")]
    Invalid(PathBuf),
    #[error("checkout path escaped managed storage: {0}")]
    Containment(PathBuf),
    #[error(
        "managed checkout root {root} is inside the source repository at {source_path}. \
         move the managed root outside that repository so provisioning cannot \
         create directories inside the source working tree"
    )]
    Nested { root: PathBuf, source_path: PathBuf },
}

/// Owns clones in `<root>/checkouts/<thread-id>/<repository-id>/<checkout-id>`
/// (legacy) and repo-local checkouts in `<source>/.trama/worktrees/`.
#[derive(Clone, Debug)]
pub struct CheckoutManager {
    pub(crate) root: PathBuf,
    pub(crate) copy: CopyOptions,
}

/// How new repo-local checkouts are materialized (`config.json` `checkouts`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CopyOptions {
    pub mode: CopyMode,
    /// Repository-relative literal paths never copied into a new checkout
    /// (ignored build output that must not be shared, caches, ...). A
    /// single-component entry is skipped while cloning; a nested one is
    /// removed from the fresh copy. Tracked paths are never removed.
    pub exclude: Vec<String>,
    /// Test hook: make every `clonefile` fail as on a volume without clones.
    #[serde(skip)]
    #[doc(hidden)]
    pub simulate_clone_failure: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyMode {
    /// Clone the whole working tree (ignored build output included) with
    /// APFS `clonefile`, falling back to `git clone` where clones are not
    /// supported.
    #[default]
    CopyOnWrite,
    /// Always `git clone` (Git hardlinks the object store; ignored files are
    /// not copied).
    Git,
}

/// How a checkout's files were produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMethod {
    CopyOnWrite,
    GitClone,
}

/// The working state of a removed checkout, saved as a Git bundle outside it
/// (see [`CheckoutManager::archive_repo_local`]).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchivedCheckout {
    pub bundle: PathBuf,
    /// Snapshot commit of the full working state (parent: the checkout's
    /// `HEAD`), as `refs/trama/archive` in the bundle.
    pub snapshot: String,
    /// The branch `HEAD` was on, or `None` when detached.
    pub head_ref: Option<String>,
    pub base_revision: String,
}

/// What exists only in a checkout and would be lost with its directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemovalReport {
    /// Paths with tracked changes or untracked non-ignored files (at most
    /// [`REPORT_FILE_LIMIT`]).
    pub changed_files: Vec<String>,
    pub changed_file_count: usize,
    /// Commits on the checkout's branches, tags or `HEAD` that the source
    /// repository does not have.
    pub commits_not_in_source: usize,
}

impl RemovalReport {
    pub fn has_unsaved_work(&self) -> bool {
        self.changed_file_count > 0 || self.commits_not_in_source > 0
    }
}

pub const REPORT_FILE_LIMIT: usize = 50;
/// Ref of the working-state snapshot inside an archive bundle.
pub const ARCHIVE_REF: &str = "refs/trama/archive";

#[derive(Clone, Debug)]
pub struct ProvisionedCheckout {
    pub path: PathBuf,
    pub base_revision: String,
}

/// A subthread's copy of a parent checkout. `base_revision` is the parent's
/// `HEAD`; `fork_commit` snapshots the parent's full working state (tracked,
/// staged, unstaged and untracked non-ignored files) inside the copy.
#[derive(Clone, Debug)]
pub struct ProvisionedCopy {
    pub path: PathBuf,
    pub base_revision: String,
    pub fork_commit: String,
}

/// A local non-bare working tree whose `.git` is on the selected directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedWorktree {
    pub path: PathBuf,
    pub origin: Option<String>,
    pub default_branch: Option<String>,
    pub head: String,
}

impl CheckoutManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            copy: CopyOptions::default(),
        }
    }

    pub fn with_copy_options(mut self, copy: CopyOptions) -> Self {
        self.copy = copy;
        self
    }

    pub fn copy_options(&self) -> &CopyOptions {
        &self.copy
    }

    /// Reject a managed root that lies inside a source repository (for example
    /// a dotfiles repo that contains Application Support). The comparison
    /// resolves only ancestors that already exist and runs before creating it.
    fn assert_not_nested(&self, source: &Path) -> Result<(), CheckoutError> {
        let canonical_source = dunce::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        let managed_root = self.root.join("checkouts");
        let canonical_managed =
            canonicalize_deepest_existing(&managed_root).map_err(|_| CheckoutError::Missing {
                path: managed_root.clone(),
            })?;
        if canonical_managed.starts_with(&canonical_source) {
            return Err(CheckoutError::Nested {
                root: managed_root,
                source_path: canonical_source,
            });
        }
        Ok(())
    }

    pub fn path(
        &self,
        thread: &ThreadId,
        repository: &RepositoryId,
        checkout: &trama_domain::ids::CheckoutId,
    ) -> Result<PathBuf, CheckoutError> {
        if !safe_id(&thread.0) || !safe_id(&repository.0) || !safe_id(&checkout.0) {
            return Err(CheckoutError::Containment(
                PathBuf::from(&thread.0)
                    .join(&repository.0)
                    .join(&checkout.0),
            ));
        }
        Ok(self
            .root
            .join("checkouts")
            .join(&thread.0)
            .join(&repository.0)
            .join(&checkout.0))
    }

    /// Clone a clean source repository, and atomically publish the clone.
    /// Git history is copied, so committed files are included. Ignored files
    /// are not copied; dirty source trees are rejected rather than silently
    /// producing a checkout with missing edits.
    pub fn provision(
        &self,
        source: &Path,
        thread: &ThreadId,
        repository: &RepositoryId,
        checkout: &trama_domain::ids::CheckoutId,
    ) -> Result<ProvisionedCheckout, CheckoutError> {
        let source = dunce::canonicalize(source)?;
        if !source.is_dir() {
            return Err(CheckoutError::Invalid(source));
        }
        self.assert_not_nested(&source)?;
        let status = git(&source, ["status", "--porcelain", "--untracked-files=all"])?;
        if !status.stdout.is_empty() {
            return Err(CheckoutError::Dirty { path: source });
        }
        let base_revision = text(git(&source, ["rev-parse", "HEAD"])?);
        let destination = self.path(thread, repository, checkout)?;
        let managed_root = self.root.join("checkouts");
        fs::create_dir_all(&managed_root)?;
        let canonical_root = dunce::canonicalize(&managed_root)?;
        let parent = destination
            .parent()
            .expect("managed destination has parent");
        fs::create_dir_all(parent)?;
        let canonical_parent = dunce::canonicalize(parent)?;
        let relative_parent = parent
            .strip_prefix(&managed_root)
            .map_err(|_| CheckoutError::Containment(destination.clone()))?;
        if !canonical_parent.starts_with(&canonical_root)
            || canonical_parent != canonical_root.join(relative_parent)
        {
            return Err(CheckoutError::Containment(destination));
        }
        if fs::symlink_metadata(&destination).is_ok() {
            self.validate(source.as_path(), &destination, &base_revision)?;
            return Ok(ProvisionedCheckout {
                path: destination,
                base_revision,
            });
        }

        let staging = parent.join(format!(".staging-{}", ulid::Ulid::new()));
        let result = (|| {
            let clone_args = [
                OsStr::new("clone"),
                OsStr::new("--no-hardlinks"),
                OsStr::new("--"),
                source.as_os_str(),
                staging.as_os_str(),
            ];
            git(&source, clone_args)?;
            git(
                &staging,
                [
                    OsStr::new("remote"),
                    OsStr::new("add"),
                    OsStr::new("local"),
                    source.as_os_str(),
                ],
            )?;
            let cloned_head = text(git(&staging, ["rev-parse", "HEAD"])?);
            if cloned_head != base_revision {
                return Err(CheckoutError::Invalid(staging.clone()));
            }
            fs::rename(&staging, &destination)?;
            Ok(())
        })();
        if result.is_err() && staging.exists() {
            // Only our unique unpublished staging directory is removed.
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
        Ok(ProvisionedCheckout {
            path: destination,
            base_revision,
        })
    }

    /// Copy a parent thread's managed checkout for a subthread and publish it
    /// atomically under `<source>/.trama/worktrees/<thread>/<checkout>`, like
    /// any repo-local checkout. The copy is a copy-on-write clone of the
    /// parent checkout (its ignored build output included; see
    /// [`CopyOptions`]) at the parent's `HEAD` (so commits the parent made are
    /// included) holding the parent's working state: tracked changes (binary
    /// included) and untracked non-ignored files. Where clones are not
    /// supported it is a `git clone` with the working-tree delta replayed and
    /// no ignored files. The parent checkout and the source repository working
    /// tree are only read (the source need not be clean); the copy's `local`
    /// remote points at `source` and `origin` is the parent's. An existing
    /// copy is validated and returned, never replaced.
    pub fn provision_copy(
        &self,
        parent_checkout: &Path,
        source: &Path,
        thread: &ThreadId,
        repository: &RepositoryId,
        checkout: &trama_domain::ids::CheckoutId,
    ) -> Result<ProvisionedCopy, CheckoutError> {
        self.provision_copy_at(parent_checkout, source, thread, repository, checkout, None)
    }

    /// [`Self::provision_copy`], but with `at` set the copy materializes a
    /// snapshot commit of the parent checkout (a turn checkpoint) instead of
    /// the parent's current state: it starts at the snapshot's parent (the
    /// parent's `HEAD` when it was taken) with the snapshot's working-state
    /// delta replayed on top. The parent's current files are never read.
    pub fn provision_copy_at(
        &self,
        parent_checkout: &Path,
        source: &Path,
        thread: &ThreadId,
        repository: &RepositoryId,
        checkout: &trama_domain::ids::CheckoutId,
        at: Option<&str>,
    ) -> Result<ProvisionedCopy, CheckoutError> {
        let parent = dunce::canonicalize(parent_checkout)
            .map_err(|_| CheckoutError::Invalid(parent_checkout.to_path_buf()))?;
        let canonical_source = dunce::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        // The parent must be a managed checkout of this source, never the
        // user's primary working tree.
        let parent_local = git_text(&parent, ["remote", "get-url", "local"])
            .map_err(|_| CheckoutError::Invalid(parent.clone()))?;
        if parent == canonical_source
            || dunce::canonicalize(&parent_local).ok().as_ref() != Some(&canonical_source)
        {
            return Err(CheckoutError::Invalid(parent));
        }
        let (source, destination) =
            self.prepare_repo_local(&canonical_source, thread, repository, checkout, false)?;
        let destination_parent = destination
            .parent()
            .ok_or_else(|| CheckoutError::Containment(destination.clone()))?
            .to_path_buf();
        if fs::symlink_metadata(&destination).is_ok() {
            let base_revision = fs::read_to_string(destination.join(".git/trama-base-commit"))
                .map_err(|_| CheckoutError::Invalid(destination.clone()))?
                .trim()
                .to_owned();
            self.validate_repo_local(&source, &destination, &base_revision)?;
            let fork_commit = git_text(
                &destination,
                ["rev-parse", "--verify", crate::changes::FORK_POINT_REF],
            )
            .map_err(|_| CheckoutError::Invalid(destination.clone()))?;
            return Ok(ProvisionedCopy {
                path: destination,
                base_revision,
                fork_commit,
            });
        }

        let (base_revision, parent_tree) = match at {
            None => (
                git_text(&parent, ["rev-parse", "HEAD"])?,
                crate::changes::snapshot_tree(&parent)?,
            ),
            Some(commit) => (
                git_text(
                    &parent,
                    ["rev-parse", "--verify", &format!("{commit}^1^{{commit}}")],
                )?,
                git_text(
                    &parent,
                    ["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
                )?,
            ),
        };
        let patch = git(
            &parent,
            [
                "diff",
                "--binary",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                base_revision.as_str(),
                parent_tree.as_str(),
            ],
        )?
        .stdout;
        let origin = git_text(&parent, ["remote", "get-url", "origin"]).ok();

        let staging = destination_parent.join(format!(".staging-{}", ulid::Ulid::new()));
        let result = (|| {
            let method = self.materialize(&parent, &staging)?;
            match method {
                CopyMethod::CopyOnWrite => {
                    // The copy already holds the parent's files (build
                    // output included) and its whole object store. Keep
                    // them untouched when they are exactly the requested
                    // state; otherwise reset to the base and replay.
                    let head = git_text(&staging, ["rev-parse", "HEAD"])?;
                    let exact = at.is_none()
                        && head == base_revision
                        && crate::changes::snapshot_tree(&staging)? == parent_tree;
                    if !exact {
                        git(&staging, ["reset", "--quiet", "--hard"])?;
                        git(&staging, ["clean", "-fdq"])?;
                        if head != base_revision {
                            git(
                                &staging,
                                ["checkout", "--quiet", "--detach", base_revision.as_str()],
                            )?;
                        }
                        if !patch.is_empty() {
                            crate::changes::apply_patch(&staging, &patch)?;
                        }
                    }
                    set_remote(&staging, "origin", origin.as_deref().map(OsStr::new))?;
                    set_remote(&staging, "local", Some(source.as_os_str()))?;
                    self.prune_copy(&staging)?;
                }
                CopyMethod::GitClone => {
                    if let Some(commit) = at {
                        // A local clone copies the whole object store, so
                        // this is a fallback for snapshots it did not bring
                        // along.
                        let spec = format!("{commit}^{{commit}}");
                        if git(&staging, ["cat-file", "-e", spec.as_str()]).is_err() {
                            git(
                                &staging,
                                [
                                    OsStr::new("fetch"),
                                    OsStr::new("--quiet"),
                                    OsStr::new("--no-tags"),
                                    parent.as_os_str(),
                                    OsStr::new(
                                        "+refs/trama/checkpoints/*:refs/trama/checkpoints/*",
                                    ),
                                ],
                            )?;
                        }
                    }
                    if git_text(&staging, ["rev-parse", "HEAD"])? != base_revision {
                        git(
                            &staging,
                            ["checkout", "--quiet", "--detach", base_revision.as_str()],
                        )?;
                    }
                    set_remote(&staging, "origin", origin.as_deref().map(OsStr::new))?;
                    set_remote(&staging, "local", Some(source.as_os_str()))?;
                    if !patch.is_empty() {
                        crate::changes::apply_patch(&staging, &patch)?;
                    }
                }
            }
            // The parent's own Trama refs (fork point, last turn,
            // checkpoints) describe the parent, not this copy.
            delete_trama_refs(&staging)?;
            fs::write(staging.join(".git/trama-base-commit"), &base_revision)?;
            let fork_commit = crate::changes::record_snapshot(
                &staging,
                crate::changes::FORK_POINT_REF,
                "trama: subthread fork point",
            )?;
            let fork_tree = git_text(&staging, ["rev-parse", &format!("{fork_commit}^{{tree}}")])?;
            if fork_tree != parent_tree {
                return Err(CheckoutError::Invalid(staging.clone()));
            }
            fs::rename(&staging, &destination)?;
            Ok(fork_commit)
        })();
        if result.is_err() && fs::symlink_metadata(&staging).is_ok() {
            // Only our unique unpublished staging directory is removed.
            let _ = fs::remove_dir_all(&staging);
        }
        let fork_commit = result?;
        Ok(ProvisionedCopy {
            path: destination,
            base_revision,
            fork_commit,
        })
    }

    /// Validate identity and origin for an already-published checkout. Never
    /// replace or clean an existing directory, since it may contain user work.
    pub fn validate(
        &self,
        source: &Path,
        checkout: &Path,
        base_revision: &str,
    ) -> Result<(), CheckoutError> {
        self.assert_not_nested(source)?;
        let managed_root_path = self.root.join("checkouts");
        let managed_root = dunce::canonicalize(&managed_root_path)?;
        let canonical_checkout = dunce::canonicalize(checkout)
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
        let relative_checkout = checkout
            .strip_prefix(&managed_root_path)
            .map_err(|_| CheckoutError::Containment(checkout.to_path_buf()))?;
        let expected_checkout = managed_root.join(relative_checkout);
        if !canonical_checkout.starts_with(&managed_root) || canonical_checkout != expected_checkout
        {
            return Err(CheckoutError::Containment(checkout.to_path_buf()));
        }
        let git_dir = git(&canonical_checkout, ["rev-parse", "--absolute-git-dir"])
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
        let git_dir = PathBuf::from(text(git_dir));
        if !git_dir.starts_with(&canonical_checkout) {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
        if git(
            &canonical_checkout,
            ["cat-file", "-e", &format!("{base_revision}^{{commit}}")],
        )
        .is_err()
        {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
        let remote = text(git(&canonical_checkout, ["remote", "get-url", "local"])?);
        let canonical_remote = dunce::canonicalize(remote)
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
        let canonical_source = dunce::canonicalize(source)
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
        if canonical_remote != canonical_source {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
        Ok(())
    }

    pub fn repo_local_path(
        &self,
        source: &Path,
        thread: &ThreadId,
        checkout: &trama_domain::ids::CheckoutId,
    ) -> Result<PathBuf, CheckoutError> {
        if !safe_id(&thread.0) || !safe_id(&checkout.0) {
            return Err(CheckoutError::Containment(source.join(".trama")));
        }
        let source = dunce::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        Ok(source
            .join(".trama/worktrees")
            .join(&thread.0)
            .join(&checkout.0))
    }

    /// Validate and prepare `<source>/.trama/worktrees/<thread>/` (owner
    /// marker, local exclude) and return the canonical source plus the
    /// checkout destination, which may already exist.
    pub(crate) fn prepare_repo_local(
        &self,
        source: &Path,
        thread: &ThreadId,
        repository: &RepositoryId,
        checkout: &trama_domain::ids::CheckoutId,
        require_clean_source: bool,
    ) -> Result<(PathBuf, PathBuf), CheckoutError> {
        if !safe_id(&repository.0) {
            return Err(CheckoutError::Containment(source.join(".trama")));
        }
        let source = dunce::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        if !source.is_dir() {
            return Err(CheckoutError::Invalid(source));
        }
        let destination = self.repo_local_path(&source, thread, checkout)?;
        let worktrees = source.join(".trama/worktrees");
        let thread_directory = worktrees.join(&thread.0);
        // Inspect every existing component before changing Git ignore metadata
        // or creating the owner marker. Never follow a known directory alias.
        for path in [
            &source.join(".trama"),
            &worktrees,
            &thread_directory,
            &destination,
        ] {
            if let Ok(metadata) = fs::symlink_metadata(path) {
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || dunce::canonicalize(path)? != *path
                {
                    return Err(CheckoutError::Containment(path.to_path_buf()));
                }
            }
        }
        // Only the committed setup overrides (`.trama/prepare`, `.trama/linked`)
        // may be tracked; anything else under `.trama` belongs to someone else.
        let tracked = git(&source, ["ls-files", "-z", "--", ".trama"])?;
        let trama = source.join(".trama");
        if tracked
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .any(|entry| !TRACKED_TRAMA_CONFIG.contains(&entry))
            || fs::symlink_metadata(&trama)
                .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            return Err(CheckoutError::Invalid(trama));
        }
        if require_clean_source {
            Self::assert_clean(&source)?;
        }
        let marker = format!("trama-repo-local-v1\n{}\n", source.display());
        let marker_path = trama.join(".trama-workspace.json");
        // A `.trama` that holds only tracked setup overrides is adopted: the
        // marker is created next to them below.
        let config_only = fs::symlink_metadata(&marker_path).is_err()
            && fs::read_dir(&trama).is_ok_and(|entries| {
                let entries: Vec<_> = entries.collect();
                !entries.is_empty()
                    && entries.into_iter().all(|entry| {
                        entry.is_ok_and(|entry| {
                            // Git-style `/` on every platform (as `ls-files` prints).
                            let mut path = b".trama/".to_vec();
                            path.extend_from_slice(entry.file_name().as_encoded_bytes());
                            TRACKED_TRAMA_CONFIG.contains(&path.as_slice())
                                && entry.file_type().is_ok_and(|kind| kind.is_file())
                        })
                    })
            });
        if fs::symlink_metadata(&trama).is_ok() && !config_only {
            if fs::symlink_metadata(&marker_path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
                || fs::read_to_string(&marker_path)
                    .map_err(CheckoutError::Io)?
                    .as_str()
                    != marker.as_str()
            {
                return Err(CheckoutError::Invalid(trama));
            }
        }
        ensure_local_exclude(&source)?;
        if !trama.exists() {
            fs::create_dir(&trama)?;
        }
        if fs::symlink_metadata(&marker_path).is_err() {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker_path)?
                .write_all(marker.as_bytes())?;
        }
        for path in [&worktrees, &worktrees.join(&thread.0)] {
            if fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
            {
                return Err(CheckoutError::Containment(path.to_path_buf()));
            }
        }
        fs::create_dir_all(&worktrees)?;
        fs::create_dir_all(worktrees.join(&thread.0))?;
        if dunce::canonicalize(&worktrees)? != worktrees
            || dunce::canonicalize(worktrees.join(&thread.0))? != worktrees.join(&thread.0)
        {
            return Err(CheckoutError::Containment(destination));
        }
        Ok((source, destination))
    }

    /// Create the thread checkout `<source>/.trama/worktrees/<thread>/<checkout>`
    /// of a clean source at its `HEAD` and publish it atomically. On APFS it
    /// is a copy-on-write clone of the whole source working tree minus
    /// `.trama/` and the configured excludes: ignored build output (`target/`,
    /// `node_modules/`, ...) comes along at almost no disk cost and keeps its
    /// timestamps, so the first build is incremental. The copy gets its own
    /// `.git` (a clone too), `local` pointing at the source and `origin` at
    /// the source's `origin`, and is reset to a clean tree. Elsewhere it is a
    /// `git clone` (object files hardlinked, ignored files not copied). An
    /// existing checkout is validated and returned, never replaced.
    pub fn provision_repo_local(
        &self,
        source: &Path,
        thread: &ThreadId,
        repository: &RepositoryId,
        checkout: &trama_domain::ids::CheckoutId,
    ) -> Result<ProvisionedCheckout, CheckoutError> {
        let (source, destination) =
            self.prepare_repo_local(source, thread, repository, checkout, true)?;
        let base_revision = text(git(&source, ["rev-parse", "HEAD"])?);
        let parent = destination
            .parent()
            .ok_or_else(|| CheckoutError::Containment(destination.clone()))?;
        if destination.exists() {
            let recorded_base = fs::read_to_string(destination.join(".git/trama-base-commit"))
                .map_err(|_| CheckoutError::Invalid(destination.clone()))?;
            let recorded_base = recorded_base.trim().to_owned();
            self.validate_repo_local(&source, &destination, &recorded_base)?;
            return Ok(ProvisionedCheckout {
                path: destination,
                base_revision: recorded_base,
            });
        }
        let staging = parent.join(format!(".staging-{}", ulid::Ulid::new()));
        let result = (|| {
            let method = self.materialize(&source, &staging)?;
            self.finish_source_copy(&source, &staging, method)?;
            if text(git(&staging, ["rev-parse", "HEAD"])?) != base_revision {
                return Err(CheckoutError::Invalid(staging.clone()));
            }
            fs::write(staging.join(".git/trama-base-commit"), &base_revision)?;
            fs::rename(&staging, &destination)?;
            Ok(())
        })();
        if result.is_err() && fs::symlink_metadata(&staging).is_ok() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
        Ok(ProvisionedCheckout {
            path: destination,
            base_revision,
        })
    }

    pub fn validate_repo_local(
        &self,
        source: &Path,
        checkout: &Path,
        base_revision: &str,
    ) -> Result<(), CheckoutError> {
        let canonical_source = dunce::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        let thread_id = checkout
            .parent()
            .and_then(Path::file_name)
            .ok_or_else(|| CheckoutError::Containment(checkout.to_path_buf()))?;
        let checkout_id = checkout
            .file_name()
            .ok_or_else(|| CheckoutError::Containment(checkout.to_path_buf()))?;
        let expected = self.repo_local_path(
            &canonical_source,
            &ThreadId::from(thread_id.to_string_lossy().as_ref()),
            &trama_domain::ids::CheckoutId::from(checkout_id.to_string_lossy().as_ref()),
        )?;
        if checkout != expected
            || dunce::canonicalize(checkout)
                .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?
                != expected
        {
            return Err(CheckoutError::Containment(checkout.to_path_buf()));
        }
        let source = canonical_source;
        let marker = source.join(".trama/.trama-workspace.json");
        let expected_marker = format!("trama-repo-local-v1\n{}\n", source.display());
        let marker_metadata = fs::symlink_metadata(&marker)
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
        if marker_metadata.file_type().is_symlink()
            || !marker_metadata.is_file()
            || fs::read_to_string(&marker).map_err(CheckoutError::Io)? != expected_marker
        {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
        let git_dir = PathBuf::from(text(git(checkout, ["rev-parse", "--absolute-git-dir"])?));
        if !git_dir.starts_with(checkout)
            || git(
                checkout,
                ["cat-file", "-e", &format!("{base_revision}^{{commit}}")],
            )
            .is_err()
        {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
        let recorded_base = fs::read_to_string(checkout.join(".git/trama-base-commit"))
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
        if recorded_base.trim() != base_revision {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
        let remote = text(git(checkout, ["remote", "get-url", "local"])?);
        if dunce::canonicalize(remote)
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?
            != source
        {
            return Err(CheckoutError::Invalid(checkout.to_path_buf()));
        }
        Ok(())
    }

    /// Require `path` itself to be an existing non-bare Git working tree root.
    /// Does not walk up to a parent repository or down into a nested one, and
    /// does not clone.
    pub fn inspect_working_root(path: &Path) -> Result<InspectedWorktree, CheckoutError> {
        if path.as_os_str().is_empty() || !path.exists() {
            return Err(CheckoutError::Missing {
                path: path.to_path_buf(),
            });
        }
        let canonical = dunce::canonicalize(path).map_err(|_| CheckoutError::Missing {
            path: path.to_path_buf(),
        })?;
        if !canonical.is_dir() {
            return Err(CheckoutError::NotWorktree { path: canonical });
        }
        if !canonical.join(".git").exists() {
            return Err(classify_non_worktree(&canonical));
        }
        match git_text(&canonical, ["rev-parse", "--is-bare-repository"]) {
            Ok(value) if value == "true" => {
                return Err(CheckoutError::Bare { path: canonical });
            }
            Ok(_) => {}
            Err(_) => return Err(CheckoutError::NotWorktree { path: canonical }),
        }
        let toplevel = match git_text(&canonical, ["rev-parse", "--show-toplevel"]) {
            Ok(value) => value,
            Err(_) => return Err(CheckoutError::NotWorktree { path: canonical }),
        };
        let toplevel_path =
            dunce::canonicalize(Path::new(&toplevel)).unwrap_or_else(|_| PathBuf::from(toplevel));
        if toplevel_path != canonical {
            return Err(CheckoutError::NotWorktree { path: canonical });
        }
        match git_text(&canonical, ["rev-parse", "--is-inside-work-tree"]) {
            Ok(value) if value == "true" => {}
            _ => return Err(CheckoutError::NotWorktree { path: canonical }),
        }
        let head = match git_text(&canonical, ["rev-parse", "HEAD"]) {
            Ok(value) if !value.is_empty() => value,
            _ => return Err(CheckoutError::NoCommit { path: canonical }),
        };
        let default_branch = match git_text(&canonical, ["rev-parse", "--abbrev-ref", "HEAD"]) {
            Ok(name) if !name.is_empty() && name != "HEAD" => Some(name),
            _ => None,
        };
        let origin = optional_origin(&canonical);
        Ok(InspectedWorktree {
            path: canonical,
            origin,
            default_branch,
            head,
        })
    }

    /// Reject a dirty source before any clone is published.
    pub fn assert_clean(source: &Path) -> Result<(), CheckoutError> {
        if !source.exists() {
            return Err(CheckoutError::Missing {
                path: source.to_path_buf(),
            });
        }
        let source = dunce::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        let status = git(&source, ["status", "--porcelain", "--untracked-files=all"])?;
        if !status.stdout.is_empty() {
            return Err(CheckoutError::Dirty { path: source });
        }
        Ok(())
    }
}

mod materialize;
use materialize::*;
pub(crate) use materialize::{git, git_text, text};

#[cfg(test)]
mod tests;
