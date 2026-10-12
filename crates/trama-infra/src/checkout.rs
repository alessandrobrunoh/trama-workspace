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
        let canonical_source = fs::canonicalize(source).map_err(|_| CheckoutError::Missing {
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
        let source = fs::canonicalize(source)?;
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
        let canonical_root = fs::canonicalize(&managed_root)?;
        let parent = destination
            .parent()
            .expect("managed destination has parent");
        fs::create_dir_all(parent)?;
        let canonical_parent = fs::canonicalize(parent)?;
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
        let parent = fs::canonicalize(parent_checkout)
            .map_err(|_| CheckoutError::Invalid(parent_checkout.to_path_buf()))?;
        let canonical_source = fs::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        // The parent must be a managed checkout of this source, never the
        // user's primary working tree.
        let parent_local = git_text(&parent, ["remote", "get-url", "local"])
            .map_err(|_| CheckoutError::Invalid(parent.clone()))?;
        if parent == canonical_source
            || fs::canonicalize(&parent_local).ok().as_ref() != Some(&canonical_source)
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
        let managed_root = fs::canonicalize(&managed_root_path)?;
        let canonical_checkout = fs::canonicalize(checkout)
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
        let canonical_remote =
            fs::canonicalize(remote).map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
        let canonical_source =
            fs::canonicalize(source).map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?;
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
        let source = fs::canonicalize(source).map_err(|_| CheckoutError::Missing {
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
        let source = fs::canonicalize(source).map_err(|_| CheckoutError::Missing {
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
                    || fs::canonicalize(path)? != *path
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
                            let path = Path::new(".trama").join(entry.file_name());
                            TRACKED_TRAMA_CONFIG.contains(&path.as_os_str().as_encoded_bytes())
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
        if fs::canonicalize(&worktrees)? != worktrees
            || fs::canonicalize(worktrees.join(&thread.0))? != worktrees.join(&thread.0)
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
        let canonical_source = fs::canonicalize(source).map_err(|_| CheckoutError::Missing {
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
            || fs::canonicalize(checkout)
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
        if fs::canonicalize(remote).map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?
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
        let canonical = fs::canonicalize(path).map_err(|_| CheckoutError::Missing {
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
            fs::canonicalize(Path::new(&toplevel)).unwrap_or_else(|_| PathBuf::from(toplevel));
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
        let source = fs::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        let status = git(&source, ["status", "--porcelain", "--untracked-files=all"])?;
        if !status.stdout.is_empty() {
            return Err(CheckoutError::Dirty { path: source });
        }
        Ok(())
    }
}

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
                Err(crate::cow::CloneError::Unsupported(std::io::Error::from(
                    std::io::ErrorKind::Unsupported,
                )))
            } else {
                crate::cow::clone_tree(origin, staging, &skip)
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
        let mut entries = crate::setup::linked_entries(staging);
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
fn plain_child(root: &Path, entry: &str) -> Option<PathBuf> {
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
fn cow_eligible(origin: &Path) -> bool {
    let dot_git = origin.join(".git");
    fs::symlink_metadata(&dot_git).is_ok_and(|metadata| metadata.is_dir())
        && fs::symlink_metadata(dot_git.join("commondir")).is_err()
        && fs::symlink_metadata(dot_git.join("objects/info/alternates")).is_err()
        && git(origin, ["config", "--local", "--get", "core.worktree"]).is_err()
}

/// True when `<dir>/.trama` holds only the tracked setup overrides, so it is
/// part of the checkout rather than Trama's managed storage.
fn trama_is_config_only(dir: &Path) -> bool {
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
fn fixup_cow_git(staging: &Path) -> Result<(), CheckoutError> {
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
        crate::setup::RECORD_FILE,
        crate::setup::LOG_FILE,
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
fn set_remote(checkout: &Path, name: &str, url: Option<&OsStr>) -> Result<(), CheckoutError> {
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

/// Repository-tracked files allowed under `.trama` (see [`crate::setup`]).
const TRACKED_TRAMA_CONFIG: [&[u8]; 2] = [b".trama/prepare", b".trama/linked"];

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn ensure_local_exclude(source: &Path) -> Result<(), CheckoutError> {
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
fn canonicalize_deepest_existing(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(path) {
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
    let mut canonical = fs::canonicalize(&existing)?;
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

fn classify_non_worktree(path: &Path) -> CheckoutError {
    match git_text(path, ["rev-parse", "--is-bare-repository"]) {
        Ok(value) if value == "true" => CheckoutError::Bare {
            path: path.to_path_buf(),
        },
        _ => CheckoutError::NotWorktree {
            path: path.to_path_buf(),
        },
    }
}

fn optional_origin(cwd: &Path) -> Option<String> {
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
        if let Ok(canonical) = fs::canonicalize(path) {
            return Some(canonical.to_string_lossy().into_owned());
        }
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(cwd: &Path, args: &[&str]) {
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

    fn repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-b", "main"]);
        fs::write(path.join("same.txt"), "original\n").unwrap();
        git(path, &["add", "same.txt"]);
        git(path, &["commit", "-m", "initial"]);
    }

    #[test]
    fn repeated_local_provision_keeps_base_and_work_after_source_advances() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let manager = CheckoutManager::new(temp.path().join("legacy"));
        let thread = ThreadId::from("thread");
        let repo = RepositoryId::from("repo");
        let checkout = trama_domain::ids::CheckoutId::from("copy");
        let first = manager
            .provision_repo_local(&source, &thread, &repo, &checkout)
            .unwrap();
        fs::write(first.path.join("same.txt"), "local uncommitted work\n").unwrap();
        fs::write(source.join("new-file.txt"), "new source commit\n").unwrap();
        git(&source, &["add", "new-file.txt"]);
        git(&source, &["commit", "-m", "advance source"]);
        git(&first.path, &["fetch", "local"]);
        let again = manager
            .provision_repo_local(&source, &thread, &repo, &checkout)
            .unwrap();
        assert_eq!(again.base_revision, first.base_revision);
        assert_eq!(again.path, first.path);
        assert_eq!(
            fs::read_to_string(again.path.join("same.txt")).unwrap(),
            "local uncommitted work\n"
        );
        assert_ne!(
            again.base_revision,
            text(super::git(&source, ["rev-parse", "HEAD"]).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn nested_alias_is_rejected_before_marker_or_exclude_writes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let trama = source.join(".trama");
        fs::create_dir(&trama).unwrap();
        let marker = trama.join(".trama-workspace.json");
        let marker_text = format!(
            "trama-repo-local-v1\n{}\n",
            source.canonicalize().unwrap().display()
        );
        fs::write(&marker, &marker_text).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, trama.join("worktrees")).unwrap();
        let exclude = source.join(".git/info/exclude");
        let before = fs::read(&exclude).unwrap();
        let manager = CheckoutManager::new(temp.path().join("legacy"));
        assert!(
            manager
                .provision_repo_local(
                    &source,
                    &ThreadId::from("thread"),
                    &RepositoryId::from("repo"),
                    &trama_domain::ids::CheckoutId::from("copy"),
                )
                .is_err()
        );
        assert_eq!(fs::read(&exclude).unwrap(), before);
        assert_eq!(fs::read_to_string(marker).unwrap(), marker_text);
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn repo_local_provisioning_isolated_and_idempotently_excluded() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let original_gitignore = fs::read(source.join(".gitignore")).ok();
        let source_status = git_text(&source, ["status", "--porcelain"]).unwrap();
        let manager = CheckoutManager::new(temp.path().join("global"));
        let thread = ThreadId::from("thread-a");
        let repo = RepositoryId::from("repo-a");
        let id = trama_domain::ids::CheckoutId::from("checkout-a");
        let checkout = manager
            .provision_repo_local(&source, &thread, &repo, &id)
            .unwrap();
        assert_eq!(
            checkout.path,
            fs::canonicalize(&source)
                .unwrap()
                .join(".trama/worktrees/thread-a/checkout-a")
        );
        fs::write(checkout.path.join("local-edit.txt"), "keep me").unwrap();
        let again = manager
            .provision_repo_local(&source, &thread, &repo, &id)
            .unwrap();
        assert_eq!(again.path, checkout.path);
        assert_eq!(
            fs::read_to_string(again.path.join("local-edit.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            git_text(&source, ["status", "--porcelain"]).unwrap(),
            source_status
        );
        assert_eq!(fs::read(source.join(".gitignore")).ok(), original_gitignore);
        let exclude =
            PathBuf::from(git_text(&source, ["rev-parse", "--git-path", "info/exclude"]).unwrap());
        let exclude = if exclude.is_absolute() {
            exclude
        } else {
            source.join(exclude)
        };
        let contents = fs::read_to_string(exclude).unwrap();
        assert_eq!(
            contents
                .lines()
                .filter(|line| line.trim() == "/.trama/")
                .count(),
            1
        );
    }

    #[test]
    fn repo_local_provisioning_rejects_tracked_trama() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        fs::create_dir_all(source.join(".trama")).unwrap();
        fs::write(source.join(".trama/owned"), "tracked").unwrap();
        git(&source, &["add", ".trama/owned"]);
        git(&source, &["commit", "-m", "tracked trama"]);
        let manager = CheckoutManager::new(temp.path().join("global"));
        assert!(
            manager
                .provision_repo_local(
                    &source,
                    &ThreadId::from("t"),
                    &RepositoryId::from("r"),
                    &trama_domain::ids::CheckoutId::from("c")
                )
                .is_err()
        );
    }

    #[test]
    fn tracked_trama_setup_overrides_are_adopted_but_nothing_else() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        fs::create_dir_all(source.join(".trama")).unwrap();
        fs::write(source.join(".trama/prepare"), "#!/bin/sh\n").unwrap();
        fs::write(source.join(".trama/linked"), ".env\n").unwrap();
        git(&source, &["add", ".trama"]);
        git(&source, &["commit", "-m", "setup overrides"]);
        let manager = CheckoutManager::new(temp.path().join("global"));
        let checkout = manager
            .provision_repo_local(
                &source,
                &ThreadId::from("t"),
                &RepositoryId::from("r"),
                &trama_domain::ids::CheckoutId::from("c"),
            )
            .unwrap();
        assert!(checkout.path.join(".trama/prepare").exists());
        assert!(source.join(".trama/.trama-workspace.json").exists());
        assert!(
            git_text(&source, ["status", "--porcelain", "--untracked-files=all"])
                .unwrap()
                .is_empty()
        );
        // A second thread reuses the adopted directory.
        manager
            .provision_repo_local(
                &source,
                &ThreadId::from("t2"),
                &RepositoryId::from("r"),
                &trama_domain::ids::CheckoutId::from("c"),
            )
            .unwrap();

        let other = temp.path().join("other");
        repository(&other);
        fs::create_dir_all(other.join(".trama")).unwrap();
        fs::write(other.join(".trama/prepare"), "#!/bin/sh\n").unwrap();
        fs::write(other.join(".trama/notes"), "foreign\n").unwrap();
        git(&other, &["add", ".trama"]);
        git(&other, &["commit", "-m", "foreign trama"]);
        assert!(
            manager
                .provision_repo_local(
                    &other,
                    &ThreadId::from("t"),
                    &RepositoryId::from("r"),
                    &trama_domain::ids::CheckoutId::from("c"),
                )
                .is_err()
        );
        assert!(!other.join(".trama/.trama-workspace.json").exists());
    }

    #[test]
    fn repo_local_provisioning_does_not_claim_foreign_trama_directory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        fs::create_dir(source.join(".trama")).unwrap();
        fs::write(source.join(".trama/foreign"), "keep").unwrap();
        let exclude =
            PathBuf::from(git_text(&source, ["rev-parse", "--git-path", "info/exclude"]).unwrap());
        let exclude = if exclude.is_absolute() {
            exclude
        } else {
            source.join(exclude)
        };
        fs::write(&exclude, "/.trama/\n").unwrap();
        let manager = CheckoutManager::new(temp.path().join("global"));
        assert!(
            manager
                .provision_repo_local(
                    &source,
                    &ThreadId::from("t"),
                    &RepositoryId::from("r"),
                    &trama_domain::ids::CheckoutId::from("c"),
                )
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(source.join(".trama/foreign")).unwrap(),
            "keep"
        );
        assert!(!source.join(".trama/.trama-workspace.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn repo_local_provisioning_rejects_symlinked_trama_root() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let foreign = temp.path().join("foreign");
        fs::create_dir_all(&foreign).unwrap();
        symlink(&foreign, source.join(".trama")).unwrap();
        let manager = CheckoutManager::new(temp.path().join("global"));
        assert!(
            manager
                .provision_repo_local(
                    &source,
                    &ThreadId::from("t"),
                    &RepositoryId::from("r"),
                    &trama_domain::ids::CheckoutId::from("c")
                )
                .is_err()
        );
        assert!(fs::read_dir(&foreign).unwrap().next().is_none());
    }

    #[test]
    fn two_threads_and_repositories_get_independent_clones_and_local_remotes() {
        let temp = tempfile::tempdir().unwrap();
        let src1 = temp.path().join("source-1");
        let src2 = temp.path().join("source-2");
        repository(&src1);
        repository(&src2);
        fs::write(src1.join(".gitignore"), ".secret\n").unwrap();
        git(&src1, &["add", ".gitignore"]);
        git(&src1, &["commit", "-m", "ignore local secret"]);
        fs::write(src1.join(".secret"), "do not copy\n").unwrap();
        let manager = CheckoutManager::new(temp.path().join("managed"));
        let t1 = ThreadId::from("thread-1");
        let t2 = ThreadId::from("thread-2");
        let r1 = RepositoryId::from("repo-1");
        let r2 = RepositoryId::from("repo-2");
        let c1 = trama_domain::ids::CheckoutId::from("checkout-1");
        let c2 = trama_domain::ids::CheckoutId::from("checkout-2");
        let c3 = trama_domain::ids::CheckoutId::from("checkout-3");
        let a = manager.provision(&src1, &t1, &r1, &c1).unwrap();
        let b = manager.provision(&src2, &t1, &r2, &c2).unwrap();
        let c = manager.provision(&src1, &t2, &r1, &c3).unwrap();
        fs::write(a.path.join("same.txt"), "thread 1 edit\n").unwrap();
        assert_eq!(
            fs::read_to_string(&c.path.join("same.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(&src1.join("same.txt")).unwrap(),
            "original\n"
        );
        assert!(!a.path.join(".secret").exists());
        assert!(src1.join(".secret").exists());
        let remote = Command::new("git")
            .arg("-C")
            .arg(&a.path)
            .args(["remote", "get-url", "local"])
            .output()
            .unwrap();
        assert!(remote.status.success());
        assert_eq!(
            String::from_utf8_lossy(&remote.stdout).trim(),
            fs::canonicalize(&src1).unwrap().to_string_lossy()
        );
        assert_ne!(a.base_revision, String::new());
        assert!(b.path.exists());
    }

    #[test]
    fn dirty_source_is_rejected_and_existing_checkout_is_validated_without_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let manager = CheckoutManager::new(temp.path().join("managed"));
        let thread = ThreadId::from("thread");
        let repo = RepositoryId::from("repo");
        let checkout_id = trama_domain::ids::CheckoutId::from("checkout");
        let checkout = manager
            .provision(&source, &thread, &repo, &checkout_id)
            .unwrap();
        fs::write(checkout.path.join("work.txt"), "preserve\n").unwrap();
        assert!(
            manager
                .provision(&source, &thread, &repo, &checkout_id)
                .is_ok()
        );
        assert_eq!(
            fs::read_to_string(checkout.path.join("work.txt")).unwrap(),
            "preserve\n"
        );
        fs::write(source.join("dirty.txt"), "dirty\n").unwrap();
        assert!(matches!(
            manager.provision(
                &source,
                &ThreadId::from("other"),
                &repo,
                &trama_domain::ids::CheckoutId::from("other-checkout")
            ),
            Err(CheckoutError::Dirty { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn validation_rejects_an_internal_symlink_to_another_threads_checkout() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let manager = CheckoutManager::new(temp.path().join("managed"));
        let repo = RepositoryId::from("repo");
        let first = manager
            .provision(
                &source,
                &ThreadId::from("thread-a"),
                &repo,
                &trama_domain::ids::CheckoutId::from("checkout-a"),
            )
            .unwrap();
        let second_path = manager
            .path(
                &ThreadId::from("thread-b"),
                &repo,
                &trama_domain::ids::CheckoutId::from("checkout-b"),
            )
            .unwrap();
        fs::create_dir_all(second_path.parent().unwrap()).unwrap();
        symlink(&first.path, &second_path).unwrap();

        assert!(
            manager
                .validate(&source, &second_path, &first.base_revision)
                .is_err()
        );
    }

    #[test]
    fn validation_rejects_when_source_and_local_remote_are_both_missing() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let manager = CheckoutManager::new(temp.path().join("managed"));
        let checkout = manager
            .provision(
                &source,
                &ThreadId::from("thread"),
                &RepositoryId::from("repo"),
                &trama_domain::ids::CheckoutId::from("checkout"),
            )
            .unwrap();
        let missing_source = temp.path().join("moved-source");
        fs::rename(&source, &missing_source).unwrap();
        fs::remove_dir_all(&missing_source).unwrap();

        assert!(
            manager
                .validate(&source, &checkout.path, &checkout.base_revision)
                .is_err()
        );
    }

    #[test]
    fn unsafe_ids_cannot_escape_managed_storage() {
        let manager = CheckoutManager::new("/tmp/trama-managed-test");
        assert!(matches!(
            manager.path(
                &ThreadId::from("../outside"),
                &RepositoryId::from("repo"),
                &trama_domain::ids::CheckoutId::from("checkout")
            ),
            Err(CheckoutError::Containment(_))
        ));
    }

    #[test]
    fn repo_local_checkouts_are_isolated_and_excluded_from_source_status() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        fs::write(source.join(".gitignore"), "ignored.local\n").unwrap();
        git(&source, &["add", ".gitignore"]);
        git(&source, &["commit", "-m", "ignore local file"]);
        fs::write(source.join("ignored.local"), "secret").unwrap();
        let manager = CheckoutManager::new(temp.path().join("unused"));
        let a = manager
            .provision_repo_local(
                &source,
                &ThreadId::from("a"),
                &RepositoryId::from("r"),
                &trama_domain::ids::CheckoutId::from("c"),
            )
            .unwrap();
        let b = manager
            .provision_repo_local(
                &source,
                &ThreadId::from("b"),
                &RepositoryId::from("r"),
                &trama_domain::ids::CheckoutId::from("c"),
            )
            .unwrap();
        assert_ne!(a.path, b.path);
        fs::write(a.path.join("same.txt"), "edited").unwrap();
        assert_eq!(
            fs::read_to_string(source.join("same.txt")).unwrap(),
            "original\n"
        );
        // Ignored files are cloned copy-on-write into the checkout.
        assert_eq!(
            fs::read_to_string(a.path.join("ignored.local")).unwrap(),
            "secret"
        );
        assert_eq!(
            fs::read_to_string(source.join(".gitignore")).unwrap(),
            "ignored.local\n"
        );
        manager
            .provision_repo_local(
                &source,
                &ThreadId::from("a"),
                &RepositoryId::from("r"),
                &trama_domain::ids::CheckoutId::from("c"),
            )
            .unwrap();
        let exclude = PathBuf::from(text(
            super::git(&source, ["rev-parse", "--git-path", "info/exclude"]).unwrap(),
        ));
        let exclude = if exclude.is_absolute() {
            exclude
        } else {
            source.join(exclude)
        };
        assert_eq!(
            fs::read_to_string(exclude)
                .unwrap()
                .matches("/.trama/")
                .count(),
            1
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["status", "--porcelain"])
                .output()
                .unwrap()
                .stdout
                .is_empty()
        );
    }

    #[test]
    fn nested_managed_root_is_rejected_before_any_directory_is_created() {
        let temp = tempfile::tempdir().unwrap();
        // A dotfiles-style setup: the source repository contains the directory
        // that the managed checkout root would live under.
        let home = temp.path().join("home-dotfiles");
        repository(&home);
        let manager = CheckoutManager::new(home.join("Library/Application Support/TramaWorkspace"));
        let err = manager
            .provision(
                &home,
                &ThreadId::from("thread"),
                &RepositoryId::from("repo"),
                &trama_domain::ids::CheckoutId::from("checkout"),
            )
            .unwrap_err();
        assert!(matches!(err, CheckoutError::Nested { .. }), "{err}");
        // Rejected before provisioning created anything inside the source.
        assert!(!home.join("Library").exists());
        let status = Command::new("git")
            .arg("-C")
            .arg(&home)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "source must stay clean"
        );

        // Even when the nested directories already exist (committed with a
        // kept file, so the source stays clean), provisioning and validation
        // still refuse.
        let nested_root = home.join("Library/Application Support/TramaWorkspace/checkouts");
        fs::create_dir_all(&nested_root).unwrap();
        fs::write(nested_root.join(".keep"), "pre-existing\n").unwrap();
        git(
            &home,
            &[
                "add",
                "Library/Application Support/TramaWorkspace/checkouts/.keep",
            ],
        );
        git(&home, &["commit", "-m", "pre-existing managed root"]);
        assert!(matches!(
            manager.provision(
                &home,
                &ThreadId::from("thread"),
                &RepositoryId::from("repo"),
                &trama_domain::ids::CheckoutId::from("checkout")
            ),
            Err(CheckoutError::Nested { .. })
        ));
        let nested = CheckoutManager::new(home.join("nested/managed"));
        assert!(matches!(
            nested.validate(
                &home,
                &home.join("nested/managed/checkouts/t/r/c"),
                "0000000000000000000000000000000000000000"
            ),
            Err(CheckoutError::Nested { .. })
        ));
    }

    #[test]
    fn managed_root_directly_inside_source_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let manager = CheckoutManager::new(source.join("checkouts"));
        assert!(matches!(
            manager.provision(
                &source,
                &ThreadId::from("thread"),
                &RepositoryId::from("repo"),
                &trama_domain::ids::CheckoutId::from("checkout")
            ),
            Err(CheckoutError::Nested { .. })
        ));
        assert!(!source.join("checkouts").exists());
    }

    #[test]
    fn inspect_accepts_only_the_non_bare_toplevel_and_optional_origin() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        git(
            &source,
            &["remote", "add", "origin", "git@example.com:org/app.git"],
        );
        let inspected = CheckoutManager::inspect_working_root(&source).unwrap();
        assert_eq!(inspected.path, fs::canonicalize(&source).unwrap());
        assert_eq!(inspected.default_branch.as_deref(), Some("main"));
        assert!(!inspected.head.is_empty());
        assert_eq!(
            inspected.origin.as_deref(),
            Some("git@example.com:org/app.git")
        );
        CheckoutManager::assert_clean(&source).unwrap();

        let child = source.join("src");
        fs::create_dir_all(&child).unwrap();
        assert!(matches!(
            CheckoutManager::inspect_working_root(&child),
            Err(CheckoutError::NotWorktree { .. })
        ));
        assert!(matches!(
            CheckoutManager::inspect_working_root(temp.path()),
            Err(CheckoutError::NotWorktree { .. })
        ));
        let missing = temp.path().join("gone");
        assert!(matches!(
            CheckoutManager::inspect_working_root(&missing),
            Err(CheckoutError::Missing { .. })
        ));

        let bare = temp.path().join("bare.git");
        fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-b", "main"]);
        assert!(matches!(
            CheckoutManager::inspect_working_root(&bare),
            Err(CheckoutError::Bare { .. })
        ));

        let file = temp.path().join("not-a-dir.txt");
        fs::write(&file, "nope\n").unwrap();
        assert!(matches!(
            CheckoutManager::inspect_working_root(&file),
            Err(CheckoutError::NotWorktree { .. })
        ));

        fs::write(source.join("dirty.txt"), "dirty\n").unwrap();
        assert!(matches!(
            CheckoutManager::assert_clean(&source),
            Err(CheckoutError::Dirty { .. })
        ));
    }
}
