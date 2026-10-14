//! Staging clones for share restore (plan `docs/plan/04`): an empty
//! repository with the manifest's object format, optionally pre-filled
//! from `origin` with the recipient's own Git credentials, so
//! [`super::share_git::restore`] finds the bundle's prerequisites.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::git::checkout::{CheckoutError, CheckoutManager, ProvisionedCheckout};
use crate::git::share_git::{ExpectedState, Result, ShareGitError};
use trama_domain::checkout_state::ObjectFormat;
use trama_domain::ids::{CheckoutId, RepositoryId, ThreadId};

fn run(cwd: Option<&Path>, args: &[&std::ffi::OsStr]) -> Result<()> {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
    // The user's own configuration and credential helpers apply; only
    // interactive prompts are disabled (the daemon has no terminal).
    let output = command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(CheckoutError::from)?;
    if !output.status.success() {
        return Err(CheckoutError::Git {
            args: args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }
        .into());
    }
    Ok(())
}

/// Creates `dir` as a fresh repository. With `origin`, fetches its branches
/// into `refs/remotes/origin/*` (no checkout, so the staging stays clean).
pub fn prepare_staging(dir: &Path, format: ObjectFormat, origin: Option<&str>) -> Result<()> {
    if dir.exists()
        && std::fs::read_dir(dir)
            .map_err(CheckoutError::from)?
            .next()
            .is_some()
    {
        return Err(ShareGitError::StagingDirty(dir.to_path_buf()));
    }
    std::fs::create_dir_all(dir).map_err(CheckoutError::from)?;
    let format = match format {
        ObjectFormat::Sha1 => "--object-format=sha1",
        ObjectFormat::Sha256 => "--object-format=sha256",
    };
    run(
        None,
        &[
            "init".as_ref(),
            "--quiet".as_ref(),
            format.as_ref(),
            dir.as_os_str(),
        ],
    )?;
    if let Some(origin) = origin {
        if origin.starts_with('-') {
            return Err(ShareGitError::InvalidComponent(origin.to_owned()));
        }
        run(
            Some(dir),
            &[
                "remote".as_ref(),
                "add".as_ref(),
                "origin".as_ref(),
                origin.as_ref(),
            ],
        )?;
        run(
            Some(dir),
            &[
                "fetch".as_ref(),
                "--quiet".as_ref(),
                "--no-tags".as_ref(),
                "origin".as_ref(),
            ],
        )?;
    }
    Ok(())
}

/// Turns a restored staging repository into the source of a regular
/// repo-local managed checkout carrying the exact shared state: the staging
/// is reset to a clean `HEAD`, a managed checkout is provisioned from it,
/// and the captured index/working state is restored into that checkout
/// from a full local bundle of the share refs (verified as usual).
pub fn materialize_share_checkout(
    manager: &CheckoutManager,
    staging: &Path,
    thread: &ThreadId,
    repository: &RepositoryId,
    checkout: &CheckoutId,
    expected: &ExpectedState,
) -> Result<ProvisionedCheckout> {
    let destination = manager
        .repo_local_path(staging, thread, checkout)
        .map_err(ShareGitError::from)?;
    if !destination.exists() {
        run(
            Some(staging),
            &[
                "read-tree".as_ref(),
                "--reset".as_ref(),
                "-u".as_ref(),
                "HEAD".as_ref(),
            ],
        )?;
        run(Some(staging), &["clean".as_ref(), "-fdq".as_ref()])?;
    }
    let provisioned = manager
        .provision_repo_local(staging, thread, repository, checkout)
        .map_err(ShareGitError::from)?;
    if provisioned.base_revision != expected.head_commit {
        return Err(ShareGitError::Mismatch {
            what: "provisioned base",
            expected: expected.head_commit.clone(),
            actual: provisioned.base_revision,
        });
    }
    let bundle = staging.join(".git").join("trama-share-local.bundle");
    run(
        Some(staging),
        &[
            "bundle".as_ref(),
            "create".as_ref(),
            "--quiet".as_ref(),
            bundle.as_os_str(),
            expected.index_ref.as_ref(),
            expected.working_ref.as_ref(),
        ],
    )?;
    let restored = super::share_git::restore(&provisioned.path, &bundle, expected);
    let _ = std::fs::remove_file(&bundle);
    restored?;
    Ok(provisioned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::share_git::{capture, export_bundle, restore};

    fn git(cwd: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn rev(cwd: &Path, spec: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", spec])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn restores_from_bundle_alone_and_with_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("a.txt"), b"one\r\nbinary\x00\n").unwrap();
        git(&src, &["add", "a.txt"]);
        git(&src, &["commit", "-qm", "one"]);
        std::fs::write(src.join("b.txt"), "staged\n").unwrap();
        git(&src, &["add", "b.txt"]);
        std::fs::write(src.join("u.txt"), "untracked\n").unwrap();
        let captured = capture(&src, "sh1", "v1").unwrap();
        let expected = ExpectedState::from(&captured);

        let full = tmp.path().join("full.bundle");
        export_bundle(&src, &captured, &[], &full).unwrap();
        let alone = tmp.path().join("alone");
        prepare_staging(&alone, captured.object_format, None).unwrap();
        restore(&alone, &full, &expected).unwrap();
        assert_eq!(rev(&alone, "HEAD"), captured.head_commit);
        assert_eq!(
            std::fs::read(alone.join("a.txt")).unwrap(),
            b"one\r\nbinary\x00\n"
        );
        assert!(alone.join("u.txt").exists());

        // The restored staging becomes the source of a managed checkout.
        let manager = CheckoutManager::new(tmp.path().join("managed"));
        let (thread, repo, co) = (
            ThreadId::from("t1"),
            RepositoryId::from("r1"),
            CheckoutId::from("c1"),
        );
        let managed =
            materialize_share_checkout(&manager, &alone, &thread, &repo, &co, &expected).unwrap();
        assert_eq!(managed.base_revision, captured.head_commit);
        manager
            .validate_repo_local(&alone, &managed.path, &managed.base_revision)
            .unwrap();
        assert_eq!(rev(&managed.path, "HEAD"), captured.head_commit);
        for f in ["a.txt", "b.txt", "u.txt"] {
            assert_eq!(
                std::fs::read(managed.path.join(f)).unwrap(),
                std::fs::read(src.join(f)).unwrap(),
                "{f}"
            );
        }
        let staged = |p: &Path| {
            let o = Command::new("git")
                .arg("-C")
                .arg(p)
                .arg("write-tree")
                .output()
                .unwrap();
            String::from_utf8(o.stdout).unwrap()
        };
        assert_eq!(staged(&managed.path), staged(&src));
        // A second run refuses: the managed checkout now carries the state.
        materialize_share_checkout(&manager, &alone, &thread, &repo, &co, &expected).unwrap_err();

        let thin = tmp.path().join("thin.bundle");
        let head = captured.head_commit.clone();
        export_bundle(&src, &captured, &[head.as_str()], &thin).unwrap();
        let fetched = tmp.path().join("fetched");
        let origin = src.to_str().unwrap();
        prepare_staging(&fetched, captured.object_format, Some(origin)).unwrap();
        restore(&fetched, &thin, &expected).unwrap();
        assert_eq!(rev(&fetched, "HEAD"), captured.head_commit);
        assert!(prepare_staging(&fetched, captured.object_format, None).is_err());
    }
}
