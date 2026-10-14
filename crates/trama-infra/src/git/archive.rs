//! Removing repo-local checkouts to free disk, and recreating them.
//!
//! A checkout is temporary: its state is its Git history plus its working
//! tree. Before a checkout directory is deleted, [`CheckoutManager::archive_repo_local`]
//! commits the full working state (tracked changes and untracked non-ignored
//! files, through a throwaway index) and writes the commits the source
//! repository lacks, with the checkout's branches, tags and Trama refs, to a
//! Git bundle under `<data>/archives/<thread>/<checkout>.bundle`.
//! [`CheckoutManager::restore_repo_local`] recreates the checkout at the same
//! path from a fresh copy of the source (copy-on-write, so build output comes
//! back too) and replays the bundle. Ignored files of the removed checkout
//! (its own build output, local databases) are not kept.

use crate::git::checkout::{
    ARCHIVE_REF, ArchivedCheckout, CheckoutError, CheckoutManager, CopyMethod, REPORT_FILE_LIMIT,
    RemovalReport, git, git_text,
};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use trama_domain::ids::{CheckoutId, RepositoryId, ThreadId};

impl CheckoutManager {
    /// Where the archive of `checkout` is kept.
    pub fn archive_path(
        &self,
        thread: &ThreadId,
        checkout: &CheckoutId,
    ) -> Result<PathBuf, CheckoutError> {
        let safe = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        };
        if !safe(&thread.0) || !safe(&checkout.0) {
            return Err(CheckoutError::Containment(self.root.join("archives")));
        }
        Ok(self
            .root
            .join("archives")
            .join(&thread.0)
            .join(format!("{}.bundle", checkout.0)))
    }

    /// What exists only in `checkout` (validated as a repo-local checkout of
    /// `source`): uncommitted or untracked files, and commits the source
    /// lacks. Reads both repositories only.
    pub fn removal_report(
        &self,
        source: &Path,
        checkout: &Path,
    ) -> Result<RemovalReport, CheckoutError> {
        let status = git(
            checkout,
            ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;
        let mut changed = Vec::new();
        let mut entries = status.stdout.split(|byte| *byte == 0).peekable();
        while let Some(entry) = entries.next() {
            if entry.len() < 4 {
                continue;
            }
            // Renames and copies carry the original path as the next field.
            if matches!(entry[0], b'R' | b'C') {
                entries.next();
            }
            changed.push(String::from_utf8_lossy(&entry[3..]).into_owned());
        }
        let count = changed.len();
        changed.truncate(REPORT_FILE_LIMIT);
        let mut revisions = positive_revisions(checkout)?;
        revisions.extend(source_exclusions(source, checkout)?);
        let commits = rev_list_count(checkout, &revisions)?;
        Ok(RemovalReport {
            changed_files: changed,
            changed_file_count: count,
            commits_not_in_source: commits,
        })
    }

    /// Save the working state of the repo-local checkout at `checkout` to its
    /// archive bundle (replacing an older one). The checkout keeps working;
    /// only `refs/trama/archive` is added to it.
    pub fn archive_repo_local(
        &self,
        source: &Path,
        thread: &ThreadId,
        checkout_id: &CheckoutId,
        checkout: &Path,
    ) -> Result<ArchivedCheckout, CheckoutError> {
        let base_revision = fs::read_to_string(checkout.join(".git/trama-base-commit"))
            .map_err(|_| CheckoutError::Invalid(checkout.to_path_buf()))?
            .trim()
            .to_owned();
        self.validate_repo_local(source, checkout, &base_revision)?;
        let snapshot = crate::git::changes::record_snapshot(
            checkout,
            ARCHIVE_REF,
            "trama: working state before the checkout was removed",
        )?;
        let head_ref = git_text(checkout, ["symbolic-ref", "-q", "HEAD"]).ok();
        let bundle = self.archive_path(thread, checkout_id)?;
        let directory = bundle
            .parent()
            .ok_or_else(|| CheckoutError::Containment(bundle.clone()))?;
        fs::create_dir_all(directory)?;
        let refs = git_text(
            checkout,
            [
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/",
                "refs/tags/",
                "refs/trama/",
            ],
        )?;
        let mut input = String::new();
        for reference in refs.lines().filter(|line| !line.is_empty()) {
            input.push_str(reference);
            input.push('\n');
        }
        for exclusion in source_exclusions(source, checkout)? {
            input.push_str(&exclusion);
            input.push('\n');
        }
        let staging = directory.join(format!(".{}.tmp", ulid::Ulid::new()));
        let mut child = Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(["bundle", "create", "--quiet"])
            .arg(&staging)
            .arg("--stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let _ = fs::remove_file(&staging);
            return Err(CheckoutError::Git {
                args: vec!["bundle".into(), "create".into()],
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let archived = ArchivedCheckout {
            bundle: bundle.clone(),
            snapshot,
            head_ref,
            base_revision,
        };
        fs::write(
            bundle.with_extension("json"),
            serde_json::to_vec_pretty(&archived)
                .map_err(|error| CheckoutError::Io(std::io::Error::other(error)))?,
        )?;
        fs::rename(&staging, &bundle)?;
        Ok(archived)
    }

    /// Delete a repo-local checkout directory after checking that it is
    /// exactly `<source>/.trama/worktrees/<thread>/<checkout>`. Never follows
    /// symbolic links.
    pub fn remove_repo_local(&self, source: &Path, checkout: &Path) -> Result<(), CheckoutError> {
        let source = dunce::canonicalize(source).map_err(|_| CheckoutError::Missing {
            path: source.to_path_buf(),
        })?;
        let (Some(thread), Some(id)) = (
            checkout.parent().and_then(Path::file_name),
            checkout.file_name(),
        ) else {
            return Err(CheckoutError::Containment(checkout.to_path_buf()));
        };
        let expected = self.repo_local_path(
            &source,
            &ThreadId::from(thread.to_string_lossy().as_ref()),
            &CheckoutId::from(id.to_string_lossy().as_ref()),
        )?;
        let metadata = fs::symlink_metadata(checkout)?;
        if checkout != expected
            || metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || dunce::canonicalize(checkout)? != expected
            || expected == source
        {
            return Err(CheckoutError::Containment(checkout.to_path_buf()));
        }
        fs::remove_dir_all(&expected)?;
        if let Some(thread_directory) = expected.parent() {
            // Only when empty.
            let _ = fs::remove_dir(thread_directory);
        }
        Ok(())
    }

    /// Recreate a removed repo-local checkout from `archived`: a fresh copy
    /// of `source` (copy-on-write when possible; the source may be dirty, its
    /// changes are not copied) with the archived commits fetched from the
    /// bundle, `HEAD` back on its branch, and the archived working state
    /// replayed. An existing checkout is validated and returned unchanged.
    pub fn restore_repo_local(
        &self,
        source: &Path,
        thread: &ThreadId,
        repository: &RepositoryId,
        checkout: &CheckoutId,
        archived: &ArchivedCheckout,
    ) -> Result<PathBuf, CheckoutError> {
        let (source, destination) =
            self.prepare_repo_local(source, thread, repository, checkout, false)?;
        if fs::symlink_metadata(&destination).is_ok() {
            self.validate_repo_local(&source, &destination, &archived.base_revision)?;
            return Ok(destination);
        }
        if archived.bundle != self.archive_path(thread, checkout)? {
            return Err(CheckoutError::Containment(archived.bundle.clone()));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| CheckoutError::Containment(destination.clone()))?;
        let staging = parent.join(format!(".staging-{}", ulid::Ulid::new()));
        let result = (|| {
            let method = self.materialize(&source, &staging)?;
            self.finish_source_copy(&source, &staging, method)?;
            git(&staging, ["checkout", "--quiet", "--detach"])?;
            git(
                &staging,
                [
                    OsStr::new("fetch"),
                    OsStr::new("--quiet"),
                    OsStr::new("--no-tags"),
                    OsStr::new("--update-head-ok"),
                    archived.bundle.as_os_str(),
                    OsStr::new("+refs/heads/*:refs/heads/*"),
                    OsStr::new("+refs/tags/*:refs/tags/*"),
                    OsStr::new("+refs/trama/*:refs/trama/*"),
                ],
            )?;
            let head = format!("{}^1", archived.snapshot);
            let tree = git_text(
                &staging,
                [
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{tree}}", archived.snapshot),
                ],
            )?;
            if let Some(branch) = &archived.head_ref {
                git(&staging, ["symbolic-ref", "HEAD", branch.as_str()])?;
            }
            git(&staging, ["reset", "--quiet", "--hard", head.as_str()])?;
            git(&staging, ["clean", "-fdq"])?;
            let patch = git(
                &staging,
                [
                    "diff",
                    "--binary",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-renames",
                    "HEAD",
                    tree.as_str(),
                ],
            )?
            .stdout;
            if !patch.is_empty() {
                crate::git::changes::apply_patch(&staging, &patch)?;
            }
            if crate::git::changes::snapshot_tree(&staging)? != tree {
                return Err(CheckoutError::Invalid(staging.clone()));
            }
            git(&staging, ["update-ref", "-d", ARCHIVE_REF])?;
            if method == CopyMethod::CopyOnWrite {
                self.prune_copy(&staging)?;
            }
            fs::write(
                staging.join(".git/trama-base-commit"),
                &archived.base_revision,
            )?;
            fs::rename(&staging, &destination)?;
            Ok(())
        })();
        if result.is_err() && fs::symlink_metadata(&staging).is_ok() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
        self.validate_repo_local(&source, &destination, &archived.base_revision)?;
        Ok(destination)
    }

    /// Delete an archive once its checkout was restored.
    pub fn discard_archive(&self, archived: &ArchivedCheckout) {
        let _ = fs::remove_file(&archived.bundle);
        let _ = fs::remove_file(archived.bundle.with_extension("json"));
        if let Some(directory) = archived.bundle.parent() {
            let _ = fs::remove_dir(directory);
        }
    }

    /// Read the archive record written next to a bundle.
    pub fn read_archive(
        &self,
        thread: &ThreadId,
        checkout: &CheckoutId,
    ) -> Result<ArchivedCheckout, CheckoutError> {
        let bundle = self.archive_path(thread, checkout)?;
        let raw = fs::read(bundle.with_extension("json"))?;
        let archived: ArchivedCheckout = serde_json::from_slice(&raw)
            .map_err(|error| CheckoutError::Io(std::io::Error::other(error)))?;
        if archived.bundle != bundle {
            return Err(CheckoutError::Containment(archived.bundle));
        }
        Ok(archived)
    }
}

/// `HEAD` plus every branch and tag of the checkout.
fn positive_revisions(checkout: &Path) -> Result<Vec<String>, CheckoutError> {
    let mut revisions = vec!["HEAD".to_owned()];
    let refs = git_text(
        checkout,
        [
            "for-each-ref",
            "--format=%(objectname)",
            "refs/heads/",
            "refs/tags/",
        ],
    )?;
    revisions.extend(
        refs.lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned),
    );
    Ok(revisions)
}

/// `^<commit>` for every ref tip of `source` that `checkout` also has: the
/// history both repositories share.
fn source_exclusions(source: &Path, checkout: &Path) -> Result<Vec<String>, CheckoutError> {
    let tips = git_text(
        source,
        [
            "for-each-ref",
            "--format=%(objectname) %(objecttype)",
            "refs/heads/",
            "refs/tags/",
            "refs/remotes/",
        ],
    )?;
    let mut candidates: Vec<String> = tips
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, kind)| *kind == "commit")
        .map(|(id, _)| id.to_owned())
        .collect();
    if let Ok(head) = git_text(source, ["rev-parse", "--verify", "HEAD"]) {
        candidates.push(head);
    }
    candidates.sort();
    candidates.dedup();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        for candidate in &candidates {
            writeln!(stdin, "{candidate}")?;
        }
    }
    let output = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_suffix(" commit"))
        .map(|id| format!("^{id}"))
        .collect())
}

fn rev_list_count(checkout: &Path, revisions: &[String]) -> Result<usize, CheckoutError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-list", "--count", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        for revision in revisions {
            writeln!(stdin, "{revision}")?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(CheckoutError::Git {
            args: vec!["rev-list".into(), "--count".into()],
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0))
}
