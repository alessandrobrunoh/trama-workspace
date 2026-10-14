//! Claude Code session transcripts (`~/.claude/projects/<cwd>/<id>.jsonl`)
//! and the truncated copies Trama resumes to fork a thread at a message.
//!
//! Claude Code has no documented "resume at message" flag, but it resumes
//! any transcript path with `--resume <path> --fork-session`. A transcript
//! is append-only JSON lines; conversation entries carry `uuid` and
//! `parentUuid`, and Claude rebuilds the conversation by walking
//! `parentUuid` back from the newest leaf. A fork therefore keeps exactly
//! the chain from the root to the chosen entry (the last assistant entry of
//! a completed turn): sidechains (Task subagents), later turns, abandoned
//! branches and metadata lines are dropped, and every kept line is copied
//! byte for byte. A compact boundary has no `parentUuid`, so the chain stops
//! there, which is also where Claude itself stops loading.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("transcript I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("message {0} is not in the agent's session transcript")]
    LeafNotFound(String),
    #[error(
        "the session transcript is incomplete: entry {missing} (an ancestor of the fork point) is missing"
    )]
    BrokenChain { missing: String },
    #[error("the session transcript has a cycle at entry {0}")]
    Cycle(String),
}

/// `$CLAUDE_CONFIG_DIR`, else `$HOME/.claude`.
pub fn claude_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
}

/// Claude Code's per-project directory name: the absolute cwd with every
/// character other than an ASCII letter or digit replaced by `-`.
pub fn project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

/// Session ids are UUIDs; anything else cannot name a file in a project
/// directory and is rejected rather than joined into a path.
fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// The transcript file of `session` (a session id, or an absolute path to a
/// `.jsonl` that was resumed directly) for a session that ran in `cwd`.
/// Looks in the project directory for `cwd` (as given and canonicalized),
/// then in every project directory, because long paths are shortened by
/// some Claude Code versions.
pub fn find_session_transcript(projects_dir: &Path, cwd: &Path, session: &str) -> Option<PathBuf> {
    let as_path = Path::new(session);
    if as_path.is_absolute() {
        return (as_path.extension().is_some_and(|ext| ext == "jsonl") && as_path.is_file())
            .then(|| as_path.to_path_buf());
    }
    if !safe_session_id(session) {
        return None;
    }
    let file = format!("{session}.jsonl");
    let mut candidates = vec![projects_dir.join(project_dir_name(cwd)).join(&file)];
    if let Ok(canonical) = dunce::canonicalize(cwd) {
        candidates.push(projects_dir.join(project_dir_name(&canonical)).join(&file));
    }
    if let Some(found) = candidates.into_iter().find(|path| path.is_file()) {
        return Some(found);
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(projects_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
}

struct Entry {
    raw: String,
    parent: Option<String>,
}

/// Read `sources` (earlier files win for a repeated uuid) into a uuid map.
/// Lines that are not JSON objects with a `uuid` (metadata, a partially
/// written last line) are skipped.
fn index_entries(sources: &[PathBuf]) -> Result<HashMap<String, Entry>, TranscriptError> {
    let mut entries = HashMap::new();
    for source in sources {
        let text = fs::read_to_string(source)?;
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(uuid) = value.get("uuid").and_then(|uuid| uuid.as_str()) else {
                continue;
            };
            let parent = value
                .get("parentUuid")
                .and_then(|parent| parent.as_str())
                .map(str::to_owned);
            entries.entry(uuid.to_owned()).or_insert(Entry {
                raw: line.to_owned(),
                parent,
            });
        }
    }
    Ok(entries)
}

/// The conversation chain ending at `leaf_uuid`, root first, as raw JSONL
/// (one entry per line, trailing newline). `sources` is the session's own
/// transcript first, then any transcripts it was resumed from, in case a
/// forked session file does not repeat its history.
pub fn truncate_transcript(
    sources: &[PathBuf],
    leaf_uuid: &str,
) -> Result<String, TranscriptError> {
    let entries = index_entries(sources)?;
    if !entries.contains_key(leaf_uuid) {
        return Err(TranscriptError::LeafNotFound(leaf_uuid.to_owned()));
    }
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(leaf_uuid.to_owned());
    while let Some(uuid) = current {
        if !seen.insert(uuid.clone()) {
            return Err(TranscriptError::Cycle(uuid));
        }
        let entry = entries
            .get(&uuid)
            .ok_or_else(|| TranscriptError::BrokenChain {
                missing: uuid.clone(),
            })?;
        chain.push(entry.raw.as_str());
        current = entry.parent.clone();
    }
    chain.reverse();
    let mut out = chain.join("\n");
    out.push('\n');
    Ok(out)
}

/// Write `content` to `dest` (owner-only, 0600) via a temporary file in the
/// same directory, so a reader never sees a partial transcript.
pub fn write_private(dest: &Path, content: &str) -> Result<(), TranscriptError> {
    let dir = dest
        .parent()
        .ok_or_else(|| std::io::Error::other("transcript copy has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let staging = dir.join(format!(".staging-{}", ulid::Ulid::new()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&staging)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&staging, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn with a tool call, a Task sidechain, metadata, a second turn,
    /// and a later abandoned branch.
    const FIXTURE: &str = r#"{"type":"summary","summary":"Fixture","leafUuid":"a3"}
{"type":"file-history-snapshot","messageId":"u1","snapshot":{}}
{"parentUuid":null,"isSidechain":false,"type":"user","uuid":"u1","sessionId":"s1","message":{"role":"user","content":"first"}}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","uuid":"a1","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}
{"parentUuid":"a1","isSidechain":true,"type":"user","uuid":"side1","sessionId":"s1","message":{"role":"user","content":"sub task"}}
{"parentUuid":"side1","isSidechain":true,"type":"assistant","uuid":"side2","sessionId":"s1","message":{"role":"assistant","content":"sub answer"}}
{"parentUuid":"a1","isSidechain":false,"type":"user","uuid":"t1","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}
{"parentUuid":"t1","isSidechain":false,"type":"assistant","uuid":"a2","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"done one"}]}}
{"parentUuid":"a2","isSidechain":false,"type":"system","subtype":"turn_duration","uuid":"sys1","sessionId":"s1"}
{"parentUuid":"sys1","isSidechain":false,"type":"user","uuid":"u2","sessionId":"s1","message":{"role":"user","content":"second"}}
{"parentUuid":"u2","isSidechain":false,"type":"assistant","uuid":"a3","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"done two"}]}}
{"parentUuid":"a2","isSidechain":false,"type":"user","uuid":"branch","sessionId":"s1","message":{"role":"user","content":"abandoned"}}
{"type":"assistant","uuid":"partial","parentUuid":"a3","mess"#;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn uuids(text: &str) -> Vec<String> {
        text.lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["uuid"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn cut_keeps_only_the_main_chain_up_to_the_turn_end() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(dir.path(), "s1.jsonl", FIXTURE);
        let first_turn = truncate_transcript(std::slice::from_ref(&source), "a2").unwrap();
        assert_eq!(uuids(&first_turn), ["u1", "a1", "t1", "a2"]);
        // Lines are copied verbatim, and every parent is kept before its child.
        let original: Vec<&str> = FIXTURE.lines().collect();
        for line in first_turn.lines() {
            assert!(original.contains(&line));
        }
        assert!(first_turn.ends_with('\n'));
        let second_turn = truncate_transcript(std::slice::from_ref(&source), "a3").unwrap();
        assert_eq!(
            uuids(&second_turn),
            ["u1", "a1", "t1", "a2", "sys1", "u2", "a3"]
        );
        assert!(!second_turn.contains("side1") && !second_turn.contains("abandoned"));
        assert!(matches!(
            truncate_transcript(std::slice::from_ref(&source), "nope"),
            Err(TranscriptError::LeafNotFound(_))
        ));
    }

    #[test]
    fn chain_continues_into_the_transcript_a_session_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let parent = write(dir.path(), "s1.jsonl", FIXTURE);
        // A forked session that only appended its own entries.
        let child = write(
            dir.path(),
            "s2.jsonl",
            r#"{"parentUuid":"a2","type":"user","uuid":"c1","sessionId":"s2","message":{"role":"user","content":"child"}}
{"parentUuid":"c1","type":"assistant","uuid":"c2","sessionId":"s2","message":{"role":"assistant","content":"child done"}}
"#,
        );
        assert!(matches!(
            truncate_transcript(std::slice::from_ref(&child), "c2"),
            Err(TranscriptError::BrokenChain { missing }) if missing == "a2"
        ));
        let cut = truncate_transcript(&[child, parent], "c2").unwrap();
        assert_eq!(uuids(&cut), ["u1", "a1", "t1", "a2", "c1", "c2"]);
    }

    #[test]
    fn cycles_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let looped = write(
            dir.path(),
            "loop.jsonl",
            "{\"uuid\":\"x\",\"parentUuid\":\"y\"}\n{\"uuid\":\"y\",\"parentUuid\":\"x\"}\n",
        );
        assert!(matches!(
            truncate_transcript(&[looped], "x"),
            Err(TranscriptError::Cycle(_))
        ));
    }

    #[test]
    fn transcript_lookup_uses_the_project_directory_then_searches() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let cwd = Path::new("/Users/me/src/app/.trama/worktrees/T1/C1");
        assert_eq!(
            project_dir_name(cwd),
            "-Users-me-src-app--trama-worktrees-T1-C1"
        );
        let direct = projects.join(project_dir_name(cwd));
        fs::create_dir_all(&direct).unwrap();
        let file = write(&direct, "abc-123.jsonl", "{}\n");
        assert_eq!(
            find_session_transcript(&projects, cwd, "abc-123"),
            Some(file.clone())
        );
        let elsewhere = projects.join("-shortened-1a2b");
        fs::create_dir_all(&elsewhere).unwrap();
        let moved = write(&elsewhere, "def-456.jsonl", "{}\n");
        assert_eq!(
            find_session_transcript(&projects, cwd, "def-456"),
            Some(moved)
        );
        assert_eq!(find_session_transcript(&projects, cwd, "../x"), None);
        assert_eq!(find_session_transcript(&projects, cwd, "missing"), None);
        assert_eq!(
            find_session_transcript(&projects, cwd, file.to_str().unwrap()),
            Some(file)
        );
    }

    #[test]
    fn private_copy_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("forks/child.jsonl");
        write_private(&dest, "line\n").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "line\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
