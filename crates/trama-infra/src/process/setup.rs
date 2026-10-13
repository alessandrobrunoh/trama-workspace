//! Project setup for new managed checkouts: `.agents/linked` symlinks and the
//! `.agents/prepare` script (`.trama/linked` / `.trama/prepare` override them).
//!
//! Both files are read from the new checkout, so they are the versions
//! committed in the repository the user attached (plus, for a subthread copy,
//! the parent checkout's working state). Setup runs once per checkout: the
//! outcome is recorded in `<checkout>/.git/trama-prepare.json` (full output in
//! `.git/trama-prepare.log`) and an existing record is never re-run. A failure
//! is recorded, never deletes the checkout and never falls back to the
//! primary checkout. Setup refuses to run in the primary checkout or as root.
//!
//! The prepare script is executed directly (no shell, no interpolation) with
//! `cwd` = the checkout, stdin closed, a cleared environment holding only
//! [`INHERITED_ENV`] plus the `TRAMA_*` variables, and a timeout that kills
//! its whole process group.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Override paths first: an existing override wins even when it is invalid.
pub const PREPARE_PATHS: [&str; 2] = [".trama/prepare", ".agents/prepare"];
pub const LINKED_PATHS: [&str; 2] = [".trama/linked", ".agents/linked"];
pub const RECORD_FILE: &str = "trama-prepare.json";
pub const LOG_FILE: &str = "trama-prepare.log";
/// Variables passed through from the daemon's environment when set.
pub const INHERITED_ENV: [&str; 9] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Overrides [`DEFAULT_TIMEOUT`] (whole seconds, at least 1).
pub const TIMEOUT_ENV: &str = "TRAMA_PREPARE_TIMEOUT_SECS";
const TAIL_BYTES: u64 = 4096;
const SUMMARY_CHARS: usize = 400;

/// The prepare timeout from [`TIMEOUT_ENV`], or [`DEFAULT_TIMEOUT`].
pub fn timeout_from_env() -> Duration {
    std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT)
}

#[derive(Clone, Debug)]
pub struct SetupRequest<'a> {
    /// The primary checkout (the repository the user added).
    pub source: &'a Path,
    /// The newly created managed checkout.
    pub checkout: &'a Path,
    pub thread_id: &'a str,
    pub thread_title: &'a str,
    /// Workspace alias of the repository; defaults to the source directory name.
    pub repository_alias: Option<&'a str>,
    pub timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStatus {
    /// Neither a prepare script nor a linked file exists.
    NotConfigured,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareRun {
    /// Checkout-relative script path.
    pub script: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// The last bytes of combined stdout and stderr.
    pub output_tail: String,
    /// Why the script could not run (not executable, spawn error, ...).
    pub error: Option<String>,
}

impl PrepareRun {
    pub fn succeeded(&self) -> bool {
        self.error.is_none() && !self.timed_out && self.exit_code == Some(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedEntry {
    pub path: String,
    pub linked: bool,
    /// Rejection reason when `linked` is false.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRecord {
    pub status: SetupStatus,
    pub linked_file: Option<String>,
    pub linked: Vec<LinkedEntry>,
    pub prepare: Option<PrepareRun>,
    /// Informational hints (for example an `.envrc` that needs `direnv allow`).
    pub notes: Vec<String>,
}

impl SetupRecord {
    fn failed(error: String) -> Self {
        Self {
            status: SetupStatus::Failed,
            linked_file: None,
            linked: Vec::new(),
            prepare: Some(PrepareRun {
                script: String::new(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                output_tail: String::new(),
                error: Some(error),
            }),
            notes: Vec::new(),
        }
    }

    /// One line for the checkout panel: `prepare failed: <reason>: <tail>`.
    /// `None` unless setup failed.
    pub fn failure_summary(&self) -> Option<String> {
        if self.status != SetupStatus::Failed {
            return None;
        }
        let mut parts = Vec::new();
        if let Some(run) = self.prepare.as_ref().filter(|run| !run.succeeded()) {
            let reason = if let Some(error) = &run.error {
                error.clone()
            } else if run.timed_out {
                format!("{} timed out after {} ms", run.script, run.duration_ms)
            } else {
                match run.exit_code {
                    Some(code) => format!("{} exited with status {code}", run.script),
                    None => format!("{} was terminated by a signal", run.script),
                }
            };
            parts.push(reason);
            let tail = run.output_tail.trim();
            if !tail.is_empty() {
                parts.push(last_chars(tail, SUMMARY_CHARS));
            }
        }
        for entry in self.linked.iter().filter(|entry| !entry.linked) {
            parts.push(format!(
                "linked `{}`: {}",
                entry.path,
                entry.detail.as_deref().unwrap_or("rejected")
            ));
        }
        Some(format!("prepare failed: {}", parts.join(": ")))
    }
}

/// The recorded outcome for a checkout, if setup has run there.
pub fn read_record(checkout: &Path) -> Option<SetupRecord> {
    let raw = fs::read(checkout.join(".git").join(RECORD_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Create linked symlinks, then run the prepare script, in a new checkout and
/// record the outcome. Runs at most once per checkout: an existing record is
/// returned unchanged. Never deletes or replaces anything in the checkout.
pub fn run_checkout_setup(request: &SetupRequest<'_>) -> SetupRecord {
    let (source, checkout) = match (
        dunce::canonicalize(request.source),
        dunce::canonicalize(request.checkout),
    ) {
        (Ok(source), Ok(checkout)) => (source, checkout),
        _ => return SetupRecord::failed("checkout or source repository is missing".into()),
    };
    if source == checkout || source.starts_with(&checkout) {
        // Never written: the primary checkout's `.git` is not ours.
        return SetupRecord::failed("refusing to run setup in the primary checkout".into());
    }
    let git_dir = checkout.join(".git");
    if !fs::symlink_metadata(&git_dir).is_ok_and(|metadata| metadata.is_dir()) {
        return SetupRecord::failed("checkout has no .git directory".into());
    }
    if let Some(existing) = read_record(&checkout) {
        return existing;
    }
    let record = if running_as_root() {
        SetupRecord::failed("refusing to run setup scripts with root privileges".into())
    } else {
        let (linked_file, linked) = create_links(&source, &checkout);
        let prepare = find_config(&checkout, &PREPARE_PATHS)
            .map(|script| run_prepare(request, &source, &checkout, script));
        let status = if linked_file.is_none() && prepare.is_none() {
            SetupStatus::NotConfigured
        } else if linked.iter().all(|entry| entry.linked)
            && prepare.as_ref().is_none_or(PrepareRun::succeeded)
        {
            SetupStatus::Succeeded
        } else {
            SetupStatus::Failed
        };
        SetupRecord {
            status,
            linked_file,
            linked,
            prepare,
            notes: direnv_notes(&checkout),
        }
    };
    if let Ok(json) = serde_json::to_vec_pretty(&record) {
        let _ = fs::write(git_dir.join(RECORD_FILE), json);
    }
    record
}

fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The entries of the checkout's linked file (override first), without
/// validation. A copy-on-write checkout removes its cloned copies of these
/// paths before setup links them back to the primary checkout.
pub fn linked_entries(checkout: &Path) -> Vec<String> {
    let Some(relative) = find_config(checkout, &LINKED_PATHS) else {
        return Vec::new();
    };
    let file = checkout.join(relative);
    if !fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file()) {
        return Vec::new();
    }
    fs::read_to_string(&file)
        .map(|contents| {
            contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The first configured path that exists in the checkout (even if invalid).
fn find_config(checkout: &Path, candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|relative| fs::symlink_metadata(checkout.join(relative)).is_ok())
}

fn direnv_notes(checkout: &Path) -> Vec<String> {
    let has_envrc = fs::symlink_metadata(checkout.join(".envrc")).is_ok();
    let has_direnv = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("direnv").is_file()));
    if has_envrc && has_direnv {
        vec![
            "This checkout has an .envrc: review it and run `direnv allow` in the checkout. Trama never allows it automatically."
                .into(),
        ]
    } else {
        Vec::new()
    }
}

fn run_prepare(
    request: &SetupRequest<'_>,
    source: &Path,
    checkout: &Path,
    relative: &str,
) -> PrepareRun {
    let script = checkout.join(relative);
    let mut run = PrepareRun {
        script: relative.to_owned(),
        exit_code: None,
        timed_out: false,
        duration_ms: 0,
        output_tail: String::new(),
        error: None,
    };
    if let Err(error) = check_script(checkout, &script) {
        run.error = Some(format!("{relative}: {error}"));
        return run;
    }
    let log_path = checkout.join(".git").join(LOG_FILE);
    let log = match fs::File::create(&log_path) {
        Ok(log) => log,
        Err(error) => {
            run.error = Some(format!("cannot create {LOG_FILE}: {error}"));
            return run;
        }
    };
    let alias = request
        .repository_alias
        .map(str::to_owned)
        .filter(|alias| !alias.is_empty())
        .unwrap_or_else(|| {
            source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let mut command = Command::new(&script);
    command
        .current_dir(checkout)
        .env_clear()
        .stdin(Stdio::null());
    for name in INHERITED_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if std::env::var_os("PATH").is_none() {
        command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    }
    command
        .env("PWD", checkout)
        .env("TRAMA_CHECKOUT_PATH", checkout)
        .env("TRAMA_SOURCE_PATH", source)
        .env("TRAMA_THREAD_ID", request.thread_id)
        .env("TRAMA_THREAD_TITLE", request.thread_title)
        .env("TRAMA_REPOSITORY_ALIAS", alias);
    match log.try_clone() {
        Ok(stdout) => {
            command.stdout(stdout).stderr(log);
        }
        Err(error) => {
            run.error = Some(format!("cannot open {LOG_FILE}: {error}"));
            return run;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a timeout also stops its children.
        command.process_group(0);
    }
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            run.error = Some(format!("{relative} could not be started: {error}"));
            return run;
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= request.timeout => {
                run.timed_out = true;
                kill_group(&mut child);
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                run.error = Some(format!("waiting for {relative} failed: {error}"));
                kill_group(&mut child);
                break child.wait().ok();
            }
        }
    };
    run.duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    run.exit_code = status.and_then(|status| status.code());
    run.output_tail = read_tail(&log_path);
    run
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain syscall; a negative pid targets the group we created.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// The script must be a regular executable file inside the checkout; a
/// symlink is refused so a committed link cannot point at an outside binary.
fn check_script(checkout: &Path, script: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(script).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("must be a regular file, not a symlink".into());
    }
    if !metadata.is_file() {
        return Err("must be a regular file".into());
    }
    let canonical = dunce::canonicalize(script).map_err(|error| error.to_string())?;
    if !canonical.starts_with(checkout) {
        return Err("resolves outside the checkout".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("is not executable (run `chmod +x` and commit it)".into());
        }
    }
    Ok(())
}

fn read_tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn last_chars(text: &str, count: usize) -> String {
    let total = text.chars().count();
    if total <= count {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(total - count).collect();
    format!("…{tail}")
}

fn create_links(source: &Path, checkout: &Path) -> (Option<String>, Vec<LinkedEntry>) {
    let Some(relative) = find_config(checkout, &LINKED_PATHS) else {
        return (None, Vec::new());
    };
    let file = checkout.join(relative);
    let contents = match fs::symlink_metadata(&file) {
        Ok(metadata) if metadata.is_file() => fs::read_to_string(&file),
        Ok(_) => Err(std::io::Error::other("must be a regular file")),
        Err(error) => Err(error),
    };
    let contents = match contents {
        Ok(contents) => contents,
        Err(error) => {
            return (
                Some(relative.to_owned()),
                vec![LinkedEntry {
                    path: relative.to_owned(),
                    linked: false,
                    detail: Some(format!("cannot read: {error}")),
                }],
            );
        }
    };
    let entries = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match link_one(source, checkout, line) {
            Ok(()) => LinkedEntry {
                path: line.to_owned(),
                linked: true,
                detail: None,
            },
            Err(detail) => LinkedEntry {
                path: line.to_owned(),
                linked: false,
                detail: Some(detail),
            },
        })
        .collect();
    (Some(relative.to_owned()), entries)
}

/// Symlink `<checkout>/<entry>` to `<source>/<entry>` after strict checks.
fn link_one(source: &Path, checkout: &Path, entry: &str) -> Result<(), String> {
    let relative = Path::new(entry);
    if relative.is_absolute() {
        return Err("must be a relative path".into());
    }
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => components.push(part),
            Component::ParentDir => return Err("must not contain `..`".into()),
            _ => return Err("must be a plain relative path".into()),
        }
    }
    let Some(first) = components.first() else {
        return Err("empty path".into());
    };
    if entry.starts_with(':') || entry.contains(['*', '?', '[', '\\']) {
        return Err("must be a literal path (no globs or pathspec magic)".into());
    }
    if *first == ".trama" || components.iter().any(|part| *part == ".git") {
        return Err("must not point into .git or .trama".into());
    }
    let tracked = git_output(source, &["ls-files", "--", entry])?;
    if !tracked.trim().is_empty() {
        return Err("is tracked by git; only ignored paths can be linked".into());
    }
    let target = source.join(relative);
    if fs::symlink_metadata(&target).is_err() {
        return Err(format!("does not exist in {}", source.display()));
    }
    let ignored = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["check-ignore", "-q", "--", entry])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .map_err(|error| format!("git check-ignore failed: {error}"))?;
    if !ignored.success() {
        return Err("is not ignored by git in the source repository".into());
    }
    let resolved = dunce::canonicalize(&target).map_err(|error| error.to_string())?;
    if !resolved.starts_with(source)
        || resolved == source
        || resolved.starts_with(source.join(".git"))
        || resolved.starts_with(source.join(".trama"))
    {
        return Err("resolves outside the source repository".into());
    }
    let link = checkout.join(relative);
    if fs::symlink_metadata(&link).is_ok() {
        return Err("already exists in the checkout; not overwritten".into());
    }
    // Create missing parents one level at a time, refusing to traverse a
    // symlink or a non-directory so the link cannot land outside the checkout.
    let mut parent = checkout.to_path_buf();
    for part in &components[..components.len() - 1] {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "parent {} is not a plain directory in the checkout",
                    display_relative(checkout, &parent)
                ));
            }
            Ok(_) => {}
            Err(_) => fs::create_dir(&parent).map_err(|error| error.to_string())?,
        }
    }
    symlink(&resolved, &link).map_err(|error| error.to_string())
}

fn display_relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| PathBuf::from(path))
        .display()
        .to_string()
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("linked paths need a Unix host"))
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|error| format!("git failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::checkout::CheckoutManager;
    use std::os::unix::fs::PermissionsExt;
    use trama_domain::ids::{CheckoutId, RepositoryId, ThreadId};

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

    fn write(path: &Path, contents: &str, executable: bool) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A clean source repository with `files` committed (`.trama/*` forced
    /// past any exclude).
    fn source(root: &Path, files: &[(&str, &str, bool)]) -> PathBuf {
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        write(&source.join("same.txt"), "original\n", false);
        for (path, contents, executable) in files {
            write(&source.join(path), contents, *executable);
        }
        git(&source, &["add", "-f", "."]);
        git(&source, &["commit", "-q", "-m", "initial"]);
        dunce::canonicalize(&source).unwrap()
    }

    fn checkout(root: &Path, source: &Path, thread: &str) -> PathBuf {
        CheckoutManager::new(root.join("unused"))
            .provision_repo_local(
                source,
                &ThreadId::from(thread),
                &RepositoryId::from("repo"),
                &CheckoutId::from("checkout"),
            )
            .unwrap()
            .path
    }

    fn request<'a>(source: &'a Path, checkout: &'a Path, title: &'a str) -> SetupRequest<'a> {
        SetupRequest {
            source,
            checkout,
            thread_id: "thread-1",
            thread_title: title,
            repository_alias: Some("app"),
            timeout: Duration::from_secs(20),
        }
    }

    const RECORDING_PREPARE: &str = "#!/bin/sh\n\
        { pwd -P; echo \"$TRAMA_CHECKOUT_PATH\"; echo \"$TRAMA_SOURCE_PATH\"; \
        echo \"$TRAMA_THREAD_ID\"; echo \"$TRAMA_THREAD_TITLE\"; \
        echo \"$TRAMA_REPOSITORY_ALIAS\"; echo \"${GIT_DIR:-unset}\"; } > prepared.txt\n\
        echo hello from prepare\n";

    #[test]
    fn prepare_runs_once_in_the_new_checkout_with_cwd_and_env() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(temp.path(), &[(".agents/prepare", RECORDING_PREPARE, true)]);
        let checkout = checkout(temp.path(), &source, "thread-1");
        let title = "feat $(touch pwned) `id`";
        let record = run_checkout_setup(&request(&source, &checkout, title));
        assert_eq!(record.status, SetupStatus::Succeeded, "{record:?}");
        let run = record.prepare.as_ref().unwrap();
        assert_eq!(run.script, ".agents/prepare");
        assert_eq!(run.exit_code, Some(0));
        assert!(run.output_tail.contains("hello from prepare"));
        assert_eq!(record.failure_summary(), None);
        let lines: Vec<String> = fs::read_to_string(checkout.join("prepared.txt"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let checkout_text = checkout.display().to_string();
        let source_text = source.display().to_string();
        assert_eq!(
            lines,
            [
                checkout_text.as_str(),
                checkout_text.as_str(),
                source_text.as_str(),
                "thread-1",
                title,
                "app",
                "unset"
            ]
        );
        assert!(!checkout.join("pwned").exists());
        assert!(!source.join("prepared.txt").exists());
        assert_eq!(read_record(&checkout), Some(record.clone()));
        assert!(checkout.join(".git").join(LOG_FILE).exists());

        // Recorded once: a second call returns the record without re-running.
        fs::remove_file(checkout.join("prepared.txt")).unwrap();
        assert_eq!(
            run_checkout_setup(&request(&source, &checkout, title)),
            record
        );
        assert!(!checkout.join("prepared.txt").exists());
    }

    #[test]
    fn trama_override_wins_over_agents_prepare() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(
            temp.path(),
            &[
                (".agents/prepare", "#!/bin/sh\ntouch agents-ran\n", true),
                (".trama/prepare", "#!/bin/sh\ntouch trama-ran\n", true),
            ],
        );
        let checkout = checkout(temp.path(), &source, "thread-1");
        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert_eq!(record.status, SetupStatus::Succeeded, "{record:?}");
        assert_eq!(record.prepare.unwrap().script, ".trama/prepare");
        assert!(checkout.join("trama-ran").exists());
        assert!(!checkout.join("agents-ran").exists());
    }

    #[test]
    fn failure_is_recorded_without_fallback_or_removal() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(
            temp.path(),
            &[(
                ".agents/prepare",
                "#!/bin/sh\necho installing\necho boom >&2\nexit 3\n",
                true,
            )],
        );
        let checkout = checkout(temp.path(), &source, "thread-1");
        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert_eq!(record.status, SetupStatus::Failed);
        assert_eq!(record.prepare.as_ref().unwrap().exit_code, Some(3));
        let summary = record.failure_summary().unwrap();
        assert!(summary.starts_with("prepare failed: "), "{summary}");
        assert!(summary.contains("exited with status 3"), "{summary}");
        assert!(summary.contains("boom"), "{summary}");
        assert!(checkout.join("same.txt").exists());
        assert_eq!(read_record(&checkout).unwrap().status, SetupStatus::Failed);

        // An invalid override does not fall back to `.agents/prepare`.
        let temp = tempfile::tempdir().unwrap();
        let source = super::tests::source(
            temp.path(),
            &[
                (".agents/prepare", "#!/bin/sh\ntouch agents-ran\n", true),
                (".trama/prepare", "#!/bin/sh\ntouch trama-ran\n", false),
            ],
        );
        let checkout = super::tests::checkout(temp.path(), &source, "thread-1");
        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert_eq!(record.status, SetupStatus::Failed);
        assert!(
            record.failure_summary().unwrap().contains("not executable"),
            "{record:?}"
        );
        assert!(!checkout.join("agents-ran").exists());
        assert!(!checkout.join("trama-ran").exists());
    }

    #[test]
    fn primary_checkout_is_refused_and_left_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(
            temp.path(),
            &[(".agents/prepare", "#!/bin/sh\ntouch ran\n", true)],
        );
        let record = run_checkout_setup(&request(&source, &source, "t"));
        assert_eq!(record.status, SetupStatus::Failed);
        assert!(!source.join("ran").exists());
        assert!(!source.join(".git").join(RECORD_FILE).exists());
    }

    #[test]
    fn missing_configuration_is_not_configured() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(temp.path(), &[]);
        let checkout = checkout(temp.path(), &source, "thread-1");
        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert_eq!(record.status, SetupStatus::NotConfigured);
        assert_eq!(record.failure_summary(), None);
    }

    #[test]
    fn timeout_kills_the_script_and_its_children() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(
            temp.path(),
            &[(
                ".agents/prepare",
                "#!/bin/sh\necho started\nsleep 30 &\nsleep 30\n",
                true,
            )],
        );
        let checkout = checkout(temp.path(), &source, "thread-1");
        let mut request = request(&source, &checkout, "t");
        request.timeout = Duration::from_secs(3);
        let started = Instant::now();
        let record = run_checkout_setup(&request);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(record.status, SetupStatus::Failed);
        let run = record.prepare.as_ref().unwrap();
        assert!(run.timed_out);
        assert!(run.output_tail.contains("started"));
        assert!(record.failure_summary().unwrap().contains("timed out"));
    }

    #[test]
    fn linked_paths_are_validated_and_symlinked_to_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let linked = "# shared ignored paths\n\
            .env\n\
            node_modules\n\
            \n\
            ../outside\n\
            /etc/hosts\n\
            same.txt\n\
            missing.local\n\
            present.local\n\
            escape.local\n\
            ./.env\n\
            .trama/worktrees\n";
        let source = source(
            temp.path(),
            &[
                (".gitignore", "/.env\nnode_modules/\n*.local\n", false),
                (".agents/linked", linked, false),
            ],
        );
        fs::write(source.join(".env"), "SECRET=1\n").unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("present.local"), "source\n").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("escape.local")).unwrap();
        let checkout = checkout(temp.path(), &source, "thread-1");
        fs::write(checkout.join("present.local"), "checkout\n").unwrap();

        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert_eq!(record.status, SetupStatus::Failed);
        assert_eq!(record.linked_file.as_deref(), Some(".agents/linked"));
        let outcome = |path: &str| {
            record
                .linked
                .iter()
                .find(|entry| entry.path == path)
                .unwrap_or_else(|| panic!("{path} missing from {record:?}"))
                .clone()
        };
        for ok in [".env", "node_modules"] {
            assert!(outcome(ok).linked, "{:?}", outcome(ok));
            let link = checkout.join(ok);
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_link(&link).unwrap(), source.join(ok));
        }
        assert_eq!(
            fs::read_to_string(checkout.join(".env")).unwrap(),
            "SECRET=1\n"
        );
        for (path, reason) in [
            ("../outside", ".."),
            ("/etc/hosts", "relative"),
            ("same.txt", "tracked"),
            ("missing.local", "does not exist"),
            ("present.local", "already exists"),
            ("escape.local", "outside"),
            ("./.env", "plain relative"),
            (".trama/worktrees", ".trama"),
        ] {
            let entry = outcome(path);
            assert!(!entry.linked, "{entry:?}");
            assert!(
                entry.detail.as_deref().unwrap().contains(reason),
                "{entry:?}"
            );
        }
        assert_eq!(record.linked.len(), 10, "comments and blanks are skipped");
        assert_eq!(
            fs::read_to_string(checkout.join("present.local")).unwrap(),
            "checkout\n"
        );
        assert!(!temp.path().join("outside").join("hosts").exists());
        assert!(
            record
                .failure_summary()
                .unwrap()
                .contains("linked `same.txt`")
        );
    }

    #[test]
    fn linked_rejects_non_ignored_untracked_paths_and_symlinked_parents() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(
            temp.path(),
            &[
                (".gitignore", "*.local\n", false),
                (".agents/linked", "dir/inner.local\nlink/x.local\n", false),
            ],
        );
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::write(source.join("dir/inner.local"), "x").unwrap();
        fs::create_dir_all(source.join("link")).unwrap();
        fs::write(source.join("link/x.local"), "x").unwrap();
        let checkout = checkout(temp.path(), &source, "thread-1");
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        // The copy-on-write checkout cloned the ignored `link/` directory.
        // (Only on macOS; elsewhere the plain-clone fallback has no `link/`.)
        if checkout.join("link").exists() {
            fs::remove_dir_all(checkout.join("link")).unwrap();
        }
        std::os::unix::fs::symlink(&elsewhere, checkout.join("link")).unwrap();
        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert!(record.linked[0].linked, "{record:?}");
        assert!(
            fs::symlink_metadata(checkout.join("dir/inner.local"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!record.linked[1].linked);
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());

        let temp = tempfile::tempdir().unwrap();
        let source = super::tests::source(temp.path(), &[(".agents/linked", "notes.txt\n", false)]);
        // Untracked but not ignored: the source is dirty only after cloning.
        let checkout = super::tests::checkout(temp.path(), &source, "thread-1");
        fs::write(source.join("notes.txt"), "x").unwrap();
        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert!(
            record.linked[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("not ignored")
        );
    }

    #[test]
    fn linked_runs_before_prepare() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(
            temp.path(),
            &[
                (".gitignore", "/.env\n", false),
                (".agents/linked", ".env\n", false),
                (".agents/prepare", "#!/bin/sh\ncp .env seen-env\n", true),
            ],
        );
        fs::write(source.join(".env"), "A=1\n").unwrap();
        let checkout = checkout(temp.path(), &source, "thread-1");
        let record = run_checkout_setup(&request(&source, &checkout, "t"));
        assert_eq!(record.status, SetupStatus::Succeeded, "{record:?}");
        assert_eq!(
            fs::read_to_string(checkout.join("seen-env")).unwrap(),
            "A=1\n"
        );
    }

    #[test]
    fn subthread_copies_run_prepare_with_the_parent_state() {
        let temp = tempfile::tempdir().unwrap();
        let source = source(temp.path(), &[(".agents/prepare", RECORDING_PREPARE, true)]);
        let manager = CheckoutManager::new(temp.path().join("unused"));
        let parent = checkout(temp.path(), &source, "parent");
        run_checkout_setup(&request(&source, &parent, "parent"));
        assert!(parent.join("prepared.txt").exists());
        let copy = manager
            .provision_copy(
                &parent,
                &source,
                &ThreadId::from("child"),
                &RepositoryId::from("repo"),
                &CheckoutId::from("copy"),
            )
            .unwrap();
        // The parent's untracked `prepared.txt` was copied; setup still runs
        // in the copy because the record lives in the copy's own `.git`.
        assert_eq!(read_record(&copy.path), None);
        fs::remove_file(copy.path.join("prepared.txt")).unwrap();
        let record = run_checkout_setup(&request(&source, &copy.path, "child"));
        assert_eq!(record.status, SetupStatus::Succeeded, "{record:?}");
        let first = fs::read_to_string(copy.path.join("prepared.txt")).unwrap();
        assert_eq!(first.lines().next(), Some(copy.path.to_str().unwrap()));
    }
}
