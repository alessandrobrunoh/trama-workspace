//! Staging clones for share restore (plan `docs/plan/04`): an empty
//! repository with the manifest's object format, optionally pre-filled
//! from `origin` with the recipient's own Git credentials, so
//! [`super::share_git::restore`] finds the bundle's prerequisites.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::git::checkout::CheckoutError;
use crate::git::share_git::{Result, ShareGitError};
use trama_domain::checkout_state::ObjectFormat;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::share_git::{ExpectedState, capture, export_bundle, restore};

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
