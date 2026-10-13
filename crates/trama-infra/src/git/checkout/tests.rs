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
        dunce::canonicalize(&source).unwrap().display()
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
        dunce::canonicalize(&source)
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
        dunce::canonicalize(&src1).unwrap().to_string_lossy()
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
    // Ignored files are cloned copy-on-write into the checkout (APFS
    // only; other platforms fall back to a plain clone without them).
    if cfg!(target_os = "macos") {
        assert_eq!(
            fs::read_to_string(a.path.join("ignored.local")).unwrap(),
            "secret"
        );
    }
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
    assert_eq!(inspected.path, dunce::canonicalize(&source).unwrap());
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
