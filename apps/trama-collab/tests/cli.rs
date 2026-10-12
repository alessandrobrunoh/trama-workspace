//! The `trama-collab` binary: the non-loopback bind stays refused by default.

use std::process::Command;

fn collab() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trama-collab"));
    // Isolate from the developer's environment.
    for name in [
        "TRAMA_COLLAB_BEHIND_PROXY",
        "TRAMA_COLLAB_BIND",
        "TRAMA_COLLAB_DB",
        "TRAMA_COLLAB_TRUSTED_PROXIES",
        "TRAMA_COLLAB_FORWARDED_HEADER",
    ] {
        command.env_remove(name);
    }
    command
}

#[test]
fn serve_refuses_a_non_loopback_bind_without_the_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    for address in ["0.0.0.0:8787", "[::]:8787", "10.1.2.3:8787"] {
        let output = collab()
            .args(["--db"])
            .arg(dir.path().join("c.sqlite"))
            .args(["serve", "--bind", address])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{address} must be refused");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--behind-proxy"), "{stderr}");
    }
    // No database was created before the refusal.
    assert!(!dir.path().join("c.sqlite").exists());
}

#[test]
fn falsey_or_invalid_environment_values_do_not_enable_the_proxy_mode() {
    let dir = tempfile::tempdir().unwrap();
    for value in ["0", "false", "no", "off", "", "flase", "maybe"] {
        let output = collab()
            .env("TRAMA_COLLAB_BEHIND_PROXY", value)
            .args(["--db"])
            .arg(dir.path().join("c.sqlite"))
            .args(["serve", "--bind", "0.0.0.0:8787"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{value:?} must not opt in");
    }
}

#[test]
fn the_environment_opt_in_is_reported_loudly_at_startup() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    // Loopback and an ephemeral port: nothing is exposed by this test.
    let mut child = collab()
        .env("TRAMA_COLLAB_BEHIND_PROXY", "1")
        .env("TRAMA_COLLAB_TRUSTED_PROXIES", "10.0.0.0/8,fc00::/7")
        .env("TRAMA_COLLAB_BIND", "127.0.0.1:0")
        .env("TRAMA_COLLAB_DB", dir.path().join("c.sqlite"))
        .arg("serve")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(line.contains("behind a proxy"), "{line}");
    assert!(line.contains("2 network(s)"), "{line}");
}

#[test]
fn backup_writes_a_private_copy_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("c.sqlite");
    let backup = dir.path().join("snapshot.sqlite");
    let status = collab()
        .args(["--db"])
        .arg(&db)
        .args(["create-runner", "--username", "nobody", "--name", "x"])
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "unknown user is refused, db still created"
    );
    let run = |target: &std::path::Path| {
        collab()
            .args(["--db"])
            .arg(&db)
            .args(["backup", "--to"])
            .arg(target)
            .output()
            .unwrap()
    };
    assert!(run(&backup).status.success());
    assert!(!run(&backup).status.success(), "existing file is kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "backup must be owner-only");
    }
    let copy = rusqlite::Connection::open(&backup).unwrap();
    let users: i64 = copy
        .query_row("SELECT count(*) FROM auth_users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(users, 0);
}

#[test]
fn restore_replaces_the_database_and_keeps_the_previous_one() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("c.sqlite");
    let backup = dir.path().join("backups").join("snapshot.sqlite");
    let create_user = |name: &str| {
        let mut child = collab()
            .args(["--db"])
            .arg(&db)
            .args(["create-user", "--username", name, "--password-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"long-enough-password\n")
            .unwrap();
        assert!(child.wait().unwrap().success());
    };
    let users = |path: &std::path::Path| -> Vec<String> {
        let connection = rusqlite::Connection::open(path).unwrap();
        let mut statement = connection
            .prepare("SELECT username FROM auth_users ORDER BY username")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    create_user("alice");
    // `backup` creates the missing parent directory.
    assert!(
        collab()
            .args(["--db"])
            .arg(&db)
            .args(["backup", "--to"])
            .arg(&backup)
            .status()
            .unwrap()
            .success()
    );
    create_user("bob");
    assert_eq!(users(&db), ["alice", "bob"]);

    let restore = |from: &std::path::Path| {
        collab()
            .args(["--db"])
            .arg(&db)
            .args(["restore", "--from"])
            .arg(from)
            .output()
            .unwrap()
    };
    // Anything that is not a collab database is refused and changes nothing.
    let junk = dir.path().join("junk.sqlite");
    std::fs::write(&junk, "not a database").unwrap();
    assert!(!restore(&junk).status.success());
    assert_eq!(users(&db), ["alice", "bob"]);

    assert!(restore(&backup).status.success());
    assert_eq!(users(&db), ["alice"]);
    let kept: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("c.sqlite.before-restore-"))
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(users(&dir.path().join(&kept[0])), ["alice", "bob"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&db).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
}
