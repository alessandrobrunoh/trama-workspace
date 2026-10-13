//! Share preflight: refuse to share a checkout whose index has unresolved
//! merge conflicts (v1 decision: a conflicted index has no single staged
//! tree to snapshot).

use crate::git::checkout::{CheckoutError, git_text};
use std::path::Path;

/// Paths with unmerged (stage 1–3) index entries, sorted and deduplicated.
/// Empty means the share may proceed.
pub fn unmerged_paths(worktree: &Path) -> Result<Vec<String>, CheckoutError> {
    let out = git_text(worktree, ["ls-files", "--unmerged", "-z"])?;
    let mut paths: Vec<String> = out
        .split('\0')
        .filter_map(|entry| entry.split_once('\t').map(|(_, p)| p.to_owned()))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        // `merge` exits 1 on conflict; that is the point of the test.
        assert!(ok.status.success() || args[0] == "merge", "{args:?}");
    }

    #[test]
    fn detects_conflicts_and_clean_index() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        run(d, &["init", "-q", "-b", "main"]);
        std::fs::write(d.join("a b.txt"), "base\n").unwrap();
        run(d, &["add", "."]);
        run(d, &["commit", "-qm", "base"]);
        assert!(unmerged_paths(d).unwrap().is_empty());
        run(d, &["checkout", "-qb", "other"]);
        std::fs::write(d.join("a b.txt"), "other\n").unwrap();
        run(d, &["commit", "-qam", "other"]);
        run(d, &["checkout", "-q", "main"]);
        std::fs::write(d.join("a b.txt"), "main\n").unwrap();
        run(d, &["commit", "-qam", "main"]);
        run(d, &["merge", "-q", "other"]);
        assert_eq!(unmerged_paths(d).unwrap(), ["a b.txt"]);
        run(d, &["add", "a b.txt"]);
        assert!(unmerged_paths(d).unwrap().is_empty());
    }
}
