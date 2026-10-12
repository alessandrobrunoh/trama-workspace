//! Copy-on-write thread checkouts on real temporary Git repositories (APFS).
//!
//! The large test writes about 200 MB once; everything lives in temporary
//! directories that are removed at the end.
#![cfg(target_os = "macos")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use trama_domain::ids::{CheckoutId, RepositoryId, ThreadId};
use trama_infra::checkout::{CheckoutError, CheckoutManager, CopyMode, CopyOptions};
use trama_infra::cow::disk_usage;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Trama Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Trama Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn status(cwd: &Path) -> String {
    git(cwd, &["status", "--porcelain", "--untracked-files=all"])
}

fn write_random(path: &Path, bytes: usize, seed: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::File::create(path).unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ seed;
    let mut chunk = vec![0u8; 256 * 1024];
    let mut left = bytes;
    while left > 0 {
        for word in chunk.chunks_exact_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            word.copy_from_slice(&state.to_ne_bytes());
        }
        let take = left.min(chunk.len());
        file.write_all(&chunk[..take]).unwrap();
        left -= take;
    }
}

fn free_bytes(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statfs(path.as_ptr(), &mut stats) }, 0);
    stats.f_bavail * u64::from(stats.f_bsize)
}

/// A clean source repository: tracked sources, an ignored `target/` and an
/// `origin` remote.
fn source_repo(root: &Path) -> PathBuf {
    let source = root.join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    fs::write(source.join(".gitignore"), "/target/\n*.local\n").unwrap();
    fs::write(source.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
    fs::write(source.join("README.md"), "readme\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "initial"]);
    git(
        &source,
        &["remote", "add", "origin", "git@example.invalid:org/app.git"],
    );
    source.canonicalize().unwrap()
}

fn ids(thread: &str) -> (ThreadId, RepositoryId, CheckoutId) {
    (
        ThreadId::from(thread),
        RepositoryId::from("repo"),
        CheckoutId::from(format!("{thread}-c")),
    )
}

fn provision(manager: &CheckoutManager, source: &Path, thread: &str) -> PathBuf {
    let (thread, repo, checkout) = ids(thread);
    manager
        .provision_repo_local(source, &thread, &repo, &checkout)
        .unwrap()
        .path
}

#[test]
fn twenty_thread_checkouts_share_a_large_ignored_build_dir() {
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    // 200 x 1 MiB of incompressible ignored build output, written once.
    for index in 0..200 {
        write_random(
            &source.join(format!("target/debug/deps/lib{index}.rlib")),
            1 << 20,
            index,
        );
    }
    let source_status = status(&source);
    let source_head = git(&source, &["rev-parse", "HEAD"]);
    let source_usage = disk_usage(&source).unwrap();
    assert!(source_usage.apparent_bytes >= 200 << 20);

    let manager = CheckoutManager::new(temp.path().join("data"));
    let free_before = free_bytes(temp.path());
    let started = Instant::now();
    let mut checkouts = Vec::new();
    for index in 0..20 {
        checkouts.push(provision(&manager, &source, &format!("thread-{index}")));
    }
    let elapsed = started.elapsed();
    let free_after = free_bytes(temp.path());

    let mut apparent = 0;
    let mut private = 0;
    for checkout in &checkouts {
        // The ignored build output is there, so the first build is incremental.
        assert_eq!(
            fs::read_dir(checkout.join("target/debug/deps"))
                .unwrap()
                .count(),
            200
        );
        // `.trama` (other threads' checkouts) is never copied.
        assert!(!checkout.join(".trama").exists());
        assert!(status(checkout).is_empty());
        let usage = disk_usage(checkout).unwrap();
        assert!(!usage.estimated, "{usage:?}");
        apparent += usage.apparent_bytes;
        private += usage.private_bytes;
    }
    eprintln!(
        "20 checkouts in {elapsed:?} ({:?} each): apparent {} MiB, private {} KiB, free space delta {} KiB",
        elapsed / 20,
        apparent >> 20,
        private >> 10,
        free_before.saturating_sub(free_after) >> 10
    );
    assert!(apparent >= 20 * (200 << 20));
    // Well under one copy of the source for all twenty together.
    assert!(private < 10 << 20, "private bytes {private}");
    assert!(
        free_before.saturating_sub(free_after) < 100 << 20,
        "free space dropped by {}",
        free_before - free_after
    );

    // Rewriting one 1 MiB file in one copy costs about that file only.
    write_random(
        &checkouts[3].join("target/debug/deps/lib7.rlib"),
        1 << 20,
        9_999,
    );
    let changed = disk_usage(&checkouts[3]).unwrap().private_bytes;
    let untouched = disk_usage(&checkouts[4]).unwrap().private_bytes;
    assert!(changed >= untouched + (1 << 20) - 64 * 1024, "{changed}");
    assert!(changed <= untouched + (2 << 20), "{changed}");

    // The source is untouched.
    assert_eq!(status(&source), source_status);
    assert_eq!(git(&source, &["rev-parse", "HEAD"]), source_head);
    assert_eq!(
        fs::read(source.join("target/debug/deps/lib7.rlib"))
            .unwrap()
            .len(),
        1 << 20
    );
}

#[test]
fn copy_on_write_checkout_is_an_independent_clean_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    fs::write(source.join("cache.local"), "ignored state\n").unwrap();
    // State a cloned `.git` must not inherit.
    git(&source, &["branch", "feature"]);
    fs::write(source.join(".git/index.lock"), "").unwrap();
    let manager = CheckoutManager::new(temp.path().join("data"));
    let checkout = provision(&manager, &source, "t1");
    // The source's lock stays; the checkout does not inherit it.
    assert!(source.join(".git/index.lock").exists());
    assert!(!checkout.join(".git/index.lock").exists());
    fs::remove_file(source.join(".git/index.lock")).unwrap();
    assert!(status(&checkout).is_empty());
    assert_eq!(
        fs::read_to_string(checkout.join("cache.local")).unwrap(),
        "ignored state\n"
    );
    assert_eq!(
        git(&checkout, &["remote", "get-url", "origin"]),
        "git@example.invalid:org/app.git"
    );
    assert_eq!(
        git(&checkout, &["remote", "get-url", "local"]),
        source.to_string_lossy()
    );
    // Its own object store: a commit in the checkout never reaches the source.
    fs::write(checkout.join("src/lib.rs"), "pub fn one() -> u8 { 2 }\n").unwrap();
    git(&checkout, &["commit", "-q", "-am", "thread work"]);
    let commit = git(&checkout, &["rev-parse", "HEAD"]);
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .output()
            .unwrap()
            .status
            .code()
            != Some(0)
    );
    assert_eq!(
        fs::read_to_string(source.join("src/lib.rs")).unwrap(),
        "pub fn one() -> u8 { 1 }\n"
    );
    assert!(status(&source).is_empty());
    // Tracked files keep their modification time (incremental builds).
    let source_mtime = fs::metadata(source.join("README.md"))
        .unwrap()
        .modified()
        .unwrap();
    let copy_mtime = fs::metadata(checkout.join("README.md"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(source_mtime, copy_mtime);
}

#[test]
fn dirty_source_is_still_rejected_and_nothing_leaks_into_a_new_thread() {
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    let manager = CheckoutManager::new(temp.path().join("data"));
    fs::write(source.join("README.md"), "uncommitted\n").unwrap();
    let (thread, repo, checkout) = ids("dirty");
    assert!(matches!(
        manager.provision_repo_local(&source, &thread, &repo, &checkout),
        Err(CheckoutError::Dirty { .. })
    ));
    assert!(!source.join(".trama/worktrees/dirty/dirty-c").exists());
    git(&source, &["checkout", "--", "README.md"]);
    let clean = provision(&manager, &source, "clean");
    assert_eq!(
        fs::read_to_string(clean.join("README.md")).unwrap(),
        "readme\n"
    );
}

#[test]
fn subthread_copy_keeps_the_parent_state_and_build_output() {
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    write_random(&source.join("target/out.bin"), 4 << 20, 1);
    let manager = CheckoutManager::new(temp.path().join("data"));
    let parent = provision(&manager, &source, "parent");
    fs::write(parent.join("src/lib.rs"), "pub fn one() -> u8 { 3 }\n").unwrap();
    fs::write(parent.join("untracked.txt"), "new\n").unwrap();
    write_random(&parent.join("target/parent-only.bin"), 1 << 20, 2);
    let parent_status = status(&parent);
    let (thread, repo, checkout) = ids("child");
    let copy = manager
        .provision_copy(&parent, &source, &thread, &repo, &checkout)
        .unwrap();
    assert_eq!(
        fs::read_to_string(copy.path.join("src/lib.rs")).unwrap(),
        "pub fn one() -> u8 { 3 }\n"
    );
    assert_eq!(
        fs::read_to_string(copy.path.join("untracked.txt")).unwrap(),
        "new\n"
    );
    assert!(copy.path.join("target/parent-only.bin").exists());
    assert_eq!(status(&parent), parent_status);
    assert_eq!(
        git(&copy.path, &["remote", "get-url", "local"]),
        source.to_string_lossy()
    );
    assert_eq!(
        git(&copy.path, &["remote", "get-url", "origin"]),
        "git@example.invalid:org/app.git"
    );
    // The parent's records do not carry over.
    assert!(!copy.path.join(".git/trama-prepare.json").exists());
    let usage = disk_usage(&copy.path).unwrap();
    assert!(usage.private_bytes < 1 << 20, "{usage:?}");
}

#[test]
fn forced_clonefile_failure_falls_back_to_a_hardlinked_git_clone() {
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    fs::write(source.join("cache.local"), "ignored\n").unwrap();
    git(&source, &["gc", "-q"]);
    let manager = CheckoutManager::new(temp.path().join("data")).with_copy_options(CopyOptions {
        simulate_clone_failure: true,
        ..CopyOptions::default()
    });
    let checkout = provision(&manager, &source, "fallback");
    assert!(status(&checkout).is_empty());
    // A Git clone: ignored files are not copied...
    assert!(!checkout.join("cache.local").exists());
    // ...and the immutable object files are hard links, not copies.
    let pack = fs::read_dir(checkout.join(".git/objects/pack"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .unwrap();
    assert!(fs::metadata(&pack).unwrap().nlink() > 1);
    assert_eq!(
        git(&checkout, &["remote", "get-url", "origin"]),
        "git@example.invalid:org/app.git"
    );
    assert_eq!(
        git(&checkout, &["remote", "get-url", "local"]),
        source.to_string_lossy()
    );
    // The source's branches are tracked as `local/*`.
    assert_eq!(
        git(&checkout, &["rev-parse", "local/main"]),
        git(&source, &["rev-parse", "HEAD"])
    );
    // `CopyMode::Git` behaves the same without any failure.
    let git_only = CheckoutManager::new(temp.path().join("data")).with_copy_options(CopyOptions {
        mode: CopyMode::Git,
        ..CopyOptions::default()
    });
    let plain = provision(&git_only, &source, "plain");
    assert!(!plain.join("cache.local").exists());
}

#[test]
fn a_linked_worktree_source_falls_back_to_git() {
    let temp = tempfile::tempdir().unwrap();
    let main = source_repo(temp.path());
    let linked = temp.path().join("linked");
    git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    let linked = linked.canonicalize().unwrap();
    fs::write(linked.join("cache.local"), "ignored\n").unwrap();
    let manager = CheckoutManager::new(temp.path().join("data"));
    let checkout = provision(&manager, &linked, "t");
    // `.git` is a real directory, never the linked worktree's gitfile.
    assert!(
        fs::symlink_metadata(checkout.join(".git"))
            .unwrap()
            .is_dir()
    );
    assert!(!checkout.join("cache.local").exists());
    fs::write(checkout.join("README.md"), "edited\n").unwrap();
    git(&checkout, &["commit", "-q", "-am", "edit"]);
    assert_eq!(
        fs::read_to_string(linked.join("README.md")).unwrap(),
        "readme\n"
    );
}

#[test]
fn linked_and_excluded_paths_are_not_cloned() {
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    fs::create_dir_all(source.join(".agents")).unwrap();
    fs::write(source.join(".agents/linked"), "shared.local\n").unwrap();
    fs::write(source.join(".gitignore"), "/target/\n*.local\n/cache/\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "linked"]);
    fs::write(source.join("shared.local"), "shared\n").unwrap();
    fs::create_dir_all(source.join("cache")).unwrap();
    fs::write(source.join("cache/big"), "skip me\n").unwrap();
    fs::create_dir_all(source.join("target/debug/incremental")).unwrap();
    fs::write(source.join("target/debug/incremental/state"), "x").unwrap();
    fs::write(source.join("target/debug/keep"), "y").unwrap();
    let manager = CheckoutManager::new(temp.path().join("data")).with_copy_options(CopyOptions {
        exclude: vec!["cache".into(), "target/debug/incremental".into()],
        ..CopyOptions::default()
    });
    let checkout = provision(&manager, &source, "t");
    assert!(!checkout.join("cache").exists());
    assert!(!checkout.join("target/debug/incremental").exists());
    assert!(checkout.join("target/debug/keep").exists());
    // Removed so setup can link it back to the primary checkout.
    assert!(fs::symlink_metadata(checkout.join("shared.local")).is_err());
    let record = trama_infra::setup::run_checkout_setup(&trama_infra::setup::SetupRequest {
        source: &source,
        checkout: &checkout,
        thread_id: "t",
        thread_title: "t",
        repository_alias: None,
        timeout: std::time::Duration::from_secs(20),
    });
    assert!(record.linked[0].linked, "{record:?}");
    assert!(
        fs::symlink_metadata(checkout.join("shared.local"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn archive_remove_and_restore_a_thread_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    write_random(&source.join("target/out.bin"), 1 << 20, 3);
    let manager = CheckoutManager::new(temp.path().join("data"));
    let (thread, repo, checkout_id) = ids("arch");
    let provisioned = manager
        .provision_repo_local(&source, &thread, &repo, &checkout_id)
        .unwrap();
    let checkout = provisioned.path.clone();
    git(&checkout, &["switch", "-q", "-c", "thread/work"]);
    fs::write(checkout.join("committed.txt"), "committed\n").unwrap();
    git(&checkout, &["add", "committed.txt"]);
    git(&checkout, &["commit", "-q", "-m", "thread commit"]);
    let thread_head = git(&checkout, &["rev-parse", "HEAD"]);
    fs::write(checkout.join("README.md"), "uncommitted edit\n").unwrap();
    fs::write(checkout.join("new.txt"), "untracked\n").unwrap();

    let report = manager.removal_report(&source, &checkout).unwrap();
    assert!(report.has_unsaved_work());
    assert_eq!(report.changed_file_count, 2, "{report:?}");
    assert_eq!(report.commits_not_in_source, 1);
    let clean = provision(&manager, &source, "clean");
    assert!(
        !manager
            .removal_report(&source, &clean)
            .unwrap()
            .has_unsaved_work()
    );

    let archived = manager
        .archive_repo_local(&source, &thread, &checkout_id, &checkout)
        .unwrap();
    assert!(archived.bundle.exists());
    assert_eq!(archived.head_ref.as_deref(), Some("refs/heads/thread/work"));
    let bundle_size = fs::metadata(&archived.bundle).unwrap().len();
    assert!(bundle_size < 64 * 1024, "bundle is {bundle_size} bytes");
    // Only the managed checkout directory can be removed.
    assert!(manager.remove_repo_local(&source, &source).is_err());
    manager.remove_repo_local(&source, &checkout).unwrap();
    assert!(!checkout.exists());

    // The source moves on and is dirty: neither leaks into the restore.
    fs::write(source.join("src/lib.rs"), "pub fn one() -> u8 { 9 }\n").unwrap();
    fs::write(source.join("stray.txt"), "source only\n").unwrap();
    let read = manager.read_archive(&thread, &checkout_id).unwrap();
    assert_eq!(read, archived);
    let restored = manager
        .restore_repo_local(&source, &thread, &repo, &checkout_id, &read)
        .unwrap();
    assert_eq!(restored, checkout);
    assert_eq!(git(&restored, &["rev-parse", "HEAD"]), thread_head);
    assert_eq!(
        git(&restored, &["symbolic-ref", "HEAD"]),
        "refs/heads/thread/work"
    );
    assert_eq!(
        fs::read_to_string(restored.join("README.md")).unwrap(),
        "uncommitted edit\n"
    );
    assert_eq!(
        fs::read_to_string(restored.join("new.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(
        fs::read_to_string(restored.join("src/lib.rs")).unwrap(),
        "pub fn one() -> u8 { 1 }\n"
    );
    assert!(!restored.join("stray.txt").exists());
    // Build output comes back with the copy-on-write clone of the source.
    assert!(restored.join("target/out.bin").exists());
    manager
        .validate_repo_local(&source, &restored, &provisioned.base_revision)
        .unwrap();
    manager.discard_archive(&read);
    assert!(!read.bundle.exists());
}

#[test]
fn removed_subthread_copy_restores_its_fork_point() {
    let temp = tempfile::tempdir().unwrap();
    let source = source_repo(temp.path());
    let manager = CheckoutManager::new(temp.path().join("data"));
    let parent = provision(&manager, &source, "parent");
    fs::write(parent.join("parent.txt"), "parent commit\n").unwrap();
    git(&parent, &["add", "parent.txt"]);
    git(&parent, &["commit", "-q", "-m", "parent only"]);
    let (thread, repo, checkout_id) = ids("child");
    let copy = manager
        .provision_copy(&parent, &source, &thread, &repo, &checkout_id)
        .unwrap();
    fs::write(copy.path.join("child.txt"), "child\n").unwrap();
    let archived = manager
        .archive_repo_local(&source, &thread, &checkout_id, &copy.path)
        .unwrap();
    manager.remove_repo_local(&source, &copy.path).unwrap();
    // Remove the parent too: the parent-only commit lives in the bundle.
    manager.remove_repo_local(&source, &parent).unwrap();
    let restored = manager
        .restore_repo_local(&source, &thread, &repo, &checkout_id, &archived)
        .unwrap();
    assert_eq!(
        git(
            &restored,
            &["rev-parse", trama_infra::changes::FORK_POINT_REF]
        ),
        copy.fork_commit
    );
    assert_eq!(
        fs::read_to_string(restored.join("parent.txt")).unwrap(),
        "parent commit\n"
    );
    assert_eq!(
        fs::read_to_string(restored.join("child.txt")).unwrap(),
        "child\n"
    );
    manager
        .validate_repo_local(&source, &restored, &copy.base_revision)
        .unwrap();
}

/// Manual measurement on a real repository (never run by default):
/// `TRAMA_COW_MEASURE_SOURCE=<clean repo> TRAMA_COW_MEASURE_COUNT=20 cargo test
/// -p trama-infra --test cow_checkouts -- --ignored --nocapture measure`.
/// Checkouts are created under the source's `.trama/worktrees/measure-*` and
/// removed at the end unless `TRAMA_COW_MEASURE_KEEP` is set.
#[test]
#[ignore]
fn measure_real_repository() {
    let Some(source) = std::env::var_os("TRAMA_COW_MEASURE_SOURCE") else {
        return;
    };
    let source = PathBuf::from(source).canonicalize().unwrap();
    let count: usize = std::env::var("TRAMA_COW_MEASURE_COUNT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let data = tempfile::tempdir().unwrap();
    let manager = CheckoutManager::new(data.path());
    let source_usage = disk_usage(&source).unwrap();
    let free_before = free_bytes(&source);
    let mut checkouts = Vec::new();
    let started = Instant::now();
    for index in 0..count {
        let one = Instant::now();
        checkouts.push(provision(&manager, &source, &format!("measure-{index}")));
        eprintln!("checkout {index}: {:?}", one.elapsed());
    }
    let elapsed = started.elapsed();
    let free_after = free_bytes(&source);
    let (mut apparent, mut private) = (0, 0);
    for checkout in &checkouts {
        let usage = disk_usage(checkout).unwrap();
        apparent += usage.apparent_bytes;
        private += usage.private_bytes;
    }
    eprintln!(
        "source apparent {} MiB; {count} checkouts in {elapsed:?} ({:?} each): apparent {} MiB, private {} KiB, free space delta {} KiB",
        source_usage.apparent_bytes >> 20,
        elapsed / count as u32,
        apparent >> 20,
        private >> 10,
        free_before.saturating_sub(free_after) >> 10
    );
    if std::env::var_os("TRAMA_COW_MEASURE_KEEP").is_none() {
        for checkout in &checkouts {
            manager.remove_repo_local(&source, checkout).unwrap();
        }
    }
}
