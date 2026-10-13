//! Secret scan of everything a checkout would export (docs/plan/06): the
//! working tree (tracked and untracked non-ignored files), the staged
//! index blobs and the blobs introduced by commits not on any remote. The
//! scan only reads; it never writes refs or objects.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use trama_domain::secrets::SecretScanReport;

/// More unpublished commits than this block the share instead of a scan
/// that could take minutes.
pub(crate) const MAX_UNPUBLISHED_COMMITS: usize = 500;

fn git(worktree: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn split_z(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

/// Reads blobs through one `git cat-file --batch`.
fn cat_blobs(worktree: &Path, oids: &[String]) -> Result<Vec<Vec<u8>>, String> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = oids.join("\n") + "\n";
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut out = Vec::new();
    child
        .stdout
        .take()
        .expect("piped")
        .read_to_end(&mut out)
        .map_err(|e| format!("git cat-file: {e}"))?;
    let _ = writer.join();
    let _ = child.wait();
    let mut blobs = Vec::with_capacity(oids.len());
    let mut rest = &out[..];
    for _ in oids {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("git cat-file: truncated")?;
        let header = String::from_utf8_lossy(&rest[..nl]).into_owned();
        rest = &rest[nl + 1..];
        let size: usize = match header.split(' ').nth(2).and_then(|s| s.parse().ok()) {
            Some(size) => size,
            None => {
                blobs.push(Vec::new()); // missing object: nothing to export
                continue;
            }
        };
        if rest.len() < size + 1 {
            return Err("git cat-file: truncated".into());
        }
        blobs.push(rest[..size].to_vec());
        rest = &rest[size + 1..];
    }
    Ok(blobs)
}

/// Scans one checkout into `report`; returns how many files/blobs were
/// scanned. Findings use repository-relative paths.
pub(crate) fn scan_checkout(worktree: &Path, report: &mut SecretScanReport) -> Result<u32, String> {
    let mut scanned = 0u32;
    // Working tree: what `capture` snapshots as the working commit.
    let files = git(
        worktree,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    for path in split_z(&files).collect::<BTreeSet<_>>() {
        match std::fs::read(worktree.join(&path)) {
            Ok(bytes) => {
                report.scan_file(&path, &bytes);
                scanned += 1;
            }
            Err(_) => continue, // deleted in the working tree
        }
    }
    // Index and unpublished commits: blobs that travel in the bundle.
    let mut blobs: Vec<(String, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    let staged = git(worktree, &["ls-files", "-z", "-s"])?;
    for entry in split_z(&staged) {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if let Some(oid) = meta.split(' ').nth(1)
            && seen.insert(oid.to_owned())
        {
            blobs.push((oid.to_owned(), path.to_owned()));
        }
    }
    let has_head = git(worktree, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok();
    if has_head {
        let list = git(worktree, &["rev-list", "HEAD", "--not", "--remotes"])?;
        let commits: Vec<String> = String::from_utf8_lossy(&list)
            .lines()
            .map(str::to_owned)
            .collect();
        if commits.len() > MAX_UNPUBLISHED_COMMITS {
            return Err(format!(
                "{} unpublished commits; push or squash them before sharing (limit {MAX_UNPUBLISHED_COMMITS})",
                commits.len()
            ));
        }
        for commit in commits {
            let diff = git(
                worktree,
                &[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--no-commit-id",
                    "--root",
                    "-m",
                    &commit,
                ],
            )?;
            let mut parts = split_z(&diff);
            while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
                let mut fields = meta.trim_start_matches(':').split(' ');
                let new_oid = fields.nth(3).unwrap_or_default().to_owned();
                if new_oid.bytes().all(|b| b == b'0') || meta.ends_with('D') {
                    continue;
                }
                if meta.contains(" R") || meta.contains(" C") {
                    // Renames carry two paths; diff-tree without -M never
                    // emits them, keep the parser honest anyway.
                    parts.next();
                }
                if seen.insert(new_oid.clone()) {
                    blobs.push((new_oid, path));
                }
            }
        }
    }
    let oids: Vec<String> = blobs.iter().map(|(oid, _)| oid.clone()).collect();
    for ((_, path), bytes) in blobs.iter().zip(cat_blobs(worktree, &oids)?) {
        report.scan_file(path, &bytes);
        scanned += 1;
    }
    report.findings.sort_by(|a, b| {
        (a.path.as_str(), a.line, a.rule as u8).cmp(&(b.path.as_str(), b.line, b.rule as u8))
    });
    report.findings.dedup();
    Ok(scanned)
}
