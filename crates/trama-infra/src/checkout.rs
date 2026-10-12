//! Per-thread managed Git clones.

use std::ffi::OsStr;
use std::fs;
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

/// Owns clones in `<root>/checkouts/<thread-id>/<repository-id>/<checkout-id>`.
#[derive(Clone, Debug)]
pub struct CheckoutManager {
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProvisionedCheckout {
    pub path: PathBuf,
    pub base_revision: String,
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
        Self { root: root.into() }
    }

    /// Reject a managed root that lies inside a source repository (for example
    /// a dotfiles repo that contains Application Support). Provisioning such a
    /// root would create directories inside the source working tree and dirty
    /// it. The comparison resolves only ancestors that already exist, so a
    /// managed root that has not been created yet is still detected through
    /// its canonical existing ancestors, and it runs before any directory is
    /// created.
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

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
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

fn git<I, S>(cwd: &Path, args: I) -> Result<Output, CheckoutError>
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

fn text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_text<I, S>(cwd: &Path, args: I) -> Result<String, CheckoutError>
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
