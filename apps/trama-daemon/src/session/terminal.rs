//! Inline composer terminals: a real PTY per terminal, running in one of the
//! thread's isolated checkouts.
//!
//! A terminal belongs to its thread and is shared by everyone subscribed to
//! it: output and exit are broadcast to the thread's subscribers. It keeps
//! running in the background until it exits, is closed, or the daemon shuts
//! down; nothing is ever started without an explicit `OpenTerminal`. Output
//! is retained in a bounded scrollback so a prompt can attach a plain-text
//! snapshot and a reconnecting client can replay it.

use super::SessionManager;
use crate::error::{DaemonError, Result};
use base64::Engine as _;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use trama_domain::ids::{MessageId, RepositoryId, ThreadId};
use trama_infra::pty::{PtyChild, PtySpec, exit_parts};
use trama_protocol::{DaemonMessage, TerminalExitStatus, TerminalSnapshot};

/// Raw output retained per terminal.
const SCROLLBACK_BYTES: usize = 256 * 1024;
const MAX_TERMINALS_PER_THREAD: usize = 8;
const MAX_TERMINALS: usize = 32;
const MAX_COMMAND_BYTES: usize = 8 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Snapshot attached to a prompt: at most this many trailing lines...
pub(super) const SNAPSHOT_LINES: usize = 200;
/// ...and at most this many bytes.
pub(super) const SNAPSHOT_BYTES: usize = 16 * 1024;
const CLOSE_GRACE: Duration = Duration::from_millis(1000);
/// After the process exits, how long to let the reader drain the PTY so the
/// exit notice follows the last output.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Every terminal of this daemon, keyed by terminal id.
#[derive(Default)]
pub(crate) struct Terminals {
    map: Mutex<HashMap<String, Arc<Terminal>>>,
}

struct Scrollback {
    bytes: VecDeque<u8>,
    seq: u64,
    /// Older output was dropped to stay within the bound.
    truncated: bool,
}

pub(super) struct Terminal {
    id: String,
    thread_id: ThreadId,
    repository_id: RepositoryId,
    cwd: PathBuf,
    command: Option<String>,
    size: Mutex<(u16, u16)>,
    pty: Mutex<PtyChild>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    scrollback: Mutex<Scrollback>,
    exit: Mutex<Option<TerminalExitStatus>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Terminal {
    fn snapshot(&self) -> TerminalSnapshot {
        let (cols, rows) = *lock(&self.size);
        TerminalSnapshot {
            terminal_id: self.id.clone(),
            thread_id: self.thread_id.clone(),
            repository_id: self.repository_id.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            cols,
            rows,
            last_seq: lock(&self.scrollback).seq,
            exit_status: *lock(&self.exit),
        }
    }

    /// Append output; returns its sequence number.
    fn record(&self, chunk: &[u8]) -> u64 {
        let mut scrollback = lock(&self.scrollback);
        scrollback.bytes.extend(chunk);
        let excess = scrollback.bytes.len().saturating_sub(SCROLLBACK_BYTES);
        if excess > 0 {
            scrollback.bytes.drain(..excess);
            scrollback.truncated = true;
        }
        scrollback.seq += 1;
        scrollback.seq
    }

    fn hang_up(&self, force: bool) {
        #[allow(unused_mut)]
        let mut pty = lock(&self.pty);
        if force {
            pty.kill_group();
        } else {
            pty.hang_up();
        }
    }
}

impl Terminals {
    fn get(&self, id: &str) -> Result<Arc<Terminal>> {
        lock(&self.map)
            .get(id)
            .cloned()
            .ok_or_else(|| DaemonError::NotFound(format!("terminal {id}")))
    }

    /// Kill every terminal (daemon shutdown).
    pub(crate) fn kill_all(&self) {
        let all: Vec<_> = lock(&self.map).drain().map(|(_, t)| t).collect();
        for terminal in all {
            terminal.hang_up(true);
        }
    }
}

/// The child's environment: the daemon's own, minus Claude Code's session
/// markers (a shell started here is not inside Claude Code) and Trama's
/// approval channel, with a usable `TERM`.
pub(super) fn terminal_env() -> Vec<(OsString, OsString)> {
    filter_env(std::env::vars_os())
}

pub(super) fn filter_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = vars
        .into_iter()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            key != "CLAUDECODE"
                && !key.starts_with("CLAUDE_CODE_")
                && key != crate::approval::ENV_SOCKET
                && key != crate::approval::ENV_TOKEN
                && key != "TERM"
        })
        .collect();
    env.push(("TERM".into(), "xterm-256color".into()));
    env
}

/// The user's login shell, else `/bin/sh`.
/// Program and arguments for a terminal: the user's shell on Unix
/// (`-c <command>` or a login shell), `%ComSpec%` (`/C <command>`) on Windows.
fn shell_invocation(command: Option<&str>) -> (PathBuf, Vec<OsString>) {
    if cfg!(windows) {
        let shell = std::env::var_os("ComSpec")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cmd.exe"));
        let args = match command {
            Some(command) => vec!["/C".into(), command.into()],
            None => Vec::new(),
        };
        (shell, args)
    } else {
        let args = match command {
            Some(command) => vec!["-c".into(), command.into()],
            None => vec!["-l".into()],
        };
        (user_shell(), args)
    }
}

fn user_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute() && shell.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 1000), rows.clamp(1, 500))
}

impl SessionManager {
    fn terminals(&self) -> &Terminals {
        &self.composer.terminals
    }

    /// Start a terminal in one of the thread's ready checkouts. Never the
    /// source repository: only validated isolated checkouts are used.
    pub async fn open_terminal(
        &self,
        thread_id: &ThreadId,
        repository_id: Option<RepositoryId>,
        command: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalSnapshot> {
        let command = command.filter(|command| !command.trim().is_empty());
        if let Some(command) = &command {
            if command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
                return Err(DaemonError::Invalid(
                    "terminal command is too long or contains NUL".into(),
                ));
            }
        }
        let checkouts = self.ready_checkouts(thread_id).await?;
        let checkout = match &repository_id {
            Some(id) => checkouts.iter().find(|c| &c.repository_id == id),
            None => checkouts.first(),
        }
        .ok_or_else(|| {
            DaemonError::Invalid(
                "this thread has no ready checkout for a terminal; prepare it first".into(),
            )
        })?
        .clone();
        {
            let map = lock(&self.terminals().map);
            if map.len() >= MAX_TERMINALS
                || map.values().filter(|t| &t.thread_id == thread_id).count()
                    >= MAX_TERMINALS_PER_THREAD
            {
                return Err(DaemonError::Invalid(
                    "too many open terminals; close one first".into(),
                ));
            }
        }
        let (cols, rows) = clamp_size(cols, rows);
        let (shell, args) = shell_invocation(command.as_deref());
        let spec = PtySpec {
            program: shell,
            args,
            cwd: checkout.path.clone(),
            env: terminal_env(),
            cols,
            rows,
        };
        let pty = PtyChild::spawn(&spec)
            .map_err(|error| DaemonError::Invalid(format!("could not start terminal: {error}")))?;
        let reader: Box<dyn std::io::Read + Send> =
            Box::new(pty.reader().map_err(DaemonError::from)?);
        let writer: Box<dyn std::io::Write + Send> =
            Box::new(pty.writer().map_err(DaemonError::from)?);
        let terminal = Arc::new(Terminal {
            id: format!("term-{}", MessageId::generate()).to_lowercase(),
            thread_id: thread_id.clone(),
            repository_id: checkout.repository_id.clone(),
            cwd: checkout.path.clone(),
            command,
            size: Mutex::new((cols, rows)),
            pty: Mutex::new(pty),
            writer: Mutex::new(writer),
            scrollback: Mutex::new(Scrollback {
                bytes: VecDeque::new(),
                seq: 0,
                truncated: false,
            }),
            exit: Mutex::new(None),
        });
        lock(&self.terminals().map).insert(terminal.id.clone(), Arc::clone(&terminal));
        self.spawn_terminal_threads(&terminal, reader);
        let snapshot = terminal.snapshot();
        self.broadcast(
            thread_id,
            DaemonMessage::TerminalOpened {
                thread_id: thread_id.clone(),
                terminal: snapshot.clone(),
            },
        );
        Ok(snapshot)
    }

    /// One blocking reader and one exit watcher per terminal. They hold the
    /// terminal until it exits, so output keeps flowing after a close until
    /// the process is gone.
    fn spawn_terminal_threads(
        &self,
        terminal: &Arc<Terminal>,
        mut reader: Box<dyn std::io::Read + Send>,
    ) {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let manager = self.clone();
        let reading = Arc::clone(terminal);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let seq = reading.record(&buf[..n]);
                        manager.broadcast(
                            &reading.thread_id,
                            DaemonMessage::TerminalOutput {
                                thread_id: reading.thread_id.clone(),
                                terminal_id: reading.id.clone(),
                                data: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
                                seq,
                                replay: false,
                            },
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    // EIO: every process holding the terminal is gone.
                    Err(_) => break,
                }
            }
            let _ = done_tx.send(());
        });
        let manager = self.clone();
        let watching = Arc::clone(terminal);
        std::thread::spawn(move || {
            let status = loop {
                // Release the PTY lock before sleeping, so close/kill can
                // take it between polls.
                let polled = lock(&watching.pty).try_wait();
                match polled {
                    Ok(Some(status)) => break exit_parts(status),
                    Ok(None) => std::thread::sleep(Duration::from_millis(40)),
                    Err(_) => break (None, None),
                }
            };
            let _ = done_rx.recv_timeout(DRAIN_GRACE);
            let status = TerminalExitStatus {
                code: status.0,
                signal: status.1,
            };
            *lock(&watching.exit) = Some(status);
            manager.broadcast(
                &watching.thread_id,
                DaemonMessage::TerminalExited {
                    thread_id: watching.thread_id.clone(),
                    terminal_id: watching.id.clone(),
                    status,
                },
            );
        });
    }

    pub async fn terminal_input(&self, terminal_id: &str, data: String) -> Result<()> {
        if data.len() > MAX_INPUT_BYTES {
            return Err(DaemonError::Invalid("terminal input is too large".into()));
        }
        let terminal = self.terminals().get(terminal_id)?;
        if lock(&terminal.exit).is_some() {
            return Err(DaemonError::Invalid(format!(
                "terminal {terminal_id} has exited"
            )));
        }
        // A full PTY buffer blocks the write; keep it off the runtime.
        tokio::task::spawn_blocking(move || {
            let mut writer = lock(&terminal.writer);
            writer.write_all(data.as_bytes())?;
            writer.flush()
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("terminal write task failed: {error}")))?
        .map_err(DaemonError::from)
    }

    pub fn resize_terminal(&self, terminal_id: &str, cols: u16, rows: u16) -> Result<()> {
        let terminal = self.terminals().get(terminal_id)?;
        let (cols, rows) = clamp_size(cols, rows);
        lock(&terminal.pty)
            .resize(cols, rows)
            .map_err(DaemonError::from)?;
        *lock(&terminal.size) = (cols, rows);
        Ok(())
    }

    /// Hang up the terminal (SIGHUP, then SIGKILL after a grace period) and
    /// forget it. Its exit is still broadcast.
    pub fn close_terminal(&self, terminal_id: &str) -> Result<()> {
        let terminal = lock(&self.terminals().map)
            .remove(terminal_id)
            .ok_or_else(|| DaemonError::NotFound(format!("terminal {terminal_id}")))?;
        stop_terminal(terminal);
        Ok(())
    }

    /// Close every terminal of a thread (for when the thread is closed).
    #[allow(dead_code)]
    pub fn close_thread_terminals(&self, thread_id: &ThreadId) {
        let closing: Vec<_> = {
            let mut map = lock(&self.terminals().map);
            let ids: Vec<String> = map
                .values()
                .filter(|t| &t.thread_id == thread_id)
                .map(|t| t.id.clone())
                .collect();
            ids.iter().filter_map(|id| map.remove(id)).collect()
        };
        for terminal in closing {
            stop_terminal(terminal);
        }
    }

    /// Kill every terminal. Called when the daemon shuts down.
    pub fn shutdown_terminals(&self) {
        self.terminals().kill_all();
    }

    pub fn list_terminals(&self, thread_id: &ThreadId) -> Vec<TerminalSnapshot> {
        let mut out: Vec<_> = lock(&self.terminals().map)
            .values()
            .filter(|t| &t.thread_id == thread_id)
            .map(|t| t.snapshot())
            .collect();
        out.sort_by(|a, b| a.terminal_id.cmp(&b.terminal_id));
        out
    }

    /// The retained scrollback as one `TerminalOutput` (`replay = true`).
    pub fn replay_terminal(&self, terminal_id: &str) -> Result<DaemonMessage> {
        let terminal = self.terminals().get(terminal_id)?;
        let scrollback = lock(&terminal.scrollback);
        let (front, back) = scrollback.bytes.as_slices();
        let mut bytes = Vec::with_capacity(scrollback.bytes.len());
        bytes.extend_from_slice(front);
        bytes.extend_from_slice(back);
        Ok(DaemonMessage::TerminalOutput {
            thread_id: terminal.thread_id.clone(),
            terminal_id: terminal.id.clone(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            seq: scrollback.seq,
            replay: true,
        })
    }

    /// A plain-text block with the terminal's recent output for a prompt.
    /// The terminal must belong to `thread_id`.
    pub(super) fn terminal_context(
        &self,
        thread_id: &ThreadId,
        terminal_id: &str,
    ) -> Result<String> {
        let terminal = self.terminals().get(terminal_id)?;
        if &terminal.thread_id != thread_id {
            return Err(DaemonError::Invalid(format!(
                "terminal {terminal_id} belongs to another thread"
            )));
        }
        let (raw, truncated) = {
            let scrollback = lock(&terminal.scrollback);
            let (front, back) = scrollback.bytes.as_slices();
            let mut bytes = front.to_vec();
            bytes.extend_from_slice(back);
            (bytes, scrollback.truncated)
        };
        let text = strip_ansi(&raw);
        let (tail, cut) = tail_lines(&text, SNAPSHOT_LINES, SNAPSHOT_BYTES);
        let status = match *lock(&terminal.exit) {
            None => "running".to_owned(),
            Some(TerminalExitStatus {
                code: Some(code), ..
            }) => format!("exited with code {code}"),
            Some(TerminalExitStatus {
                signal: Some(signal),
                ..
            }) => format!("killed by signal {signal}"),
            Some(_) => "exited".to_owned(),
        };
        let command = terminal
            .command
            .clone()
            .unwrap_or_else(|| "interactive shell".into());
        let mut out = format!(
            "<terminal-output command=\"{}\" cwd=\"{}\" status=\"{status}\">\n",
            attr(&command),
            attr(&terminal.cwd.display().to_string()),
        );
        if cut || truncated {
            out.push_str("[earlier output omitted]\n");
        }
        out.push_str(&tail);
        if !tail.is_empty() && !tail.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("</terminal-output>");
        Ok(out)
    }
}

fn stop_terminal(terminal: Arc<Terminal>) {
    if lock(&terminal.exit).is_some() {
        return;
    }
    terminal.hang_up(false);
    std::thread::spawn(move || {
        std::thread::sleep(CLOSE_GRACE);
        if lock(&terminal.exit).is_none() {
            terminal.hang_up(true);
        }
    });
}

fn attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('\n', " ")
}

/// Terminal output as plain text: escape sequences removed, carriage
/// returns and backspaces applied per line, other control characters
/// dropped.
pub(super) fn strip_ansi(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len());
    let mut line: Vec<char> = Vec::new();
    let mut col = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates, then a final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC / DCS / APC / PM: until BEL or ST (ESC \).
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Charset selection takes one more byte.
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                }
                _ => {}
            },
            '\n' => {
                out.extend(line.iter());
                out.push('\n');
                line.clear();
                col = 0;
            }
            '\r' => col = 0,
            '\u{8}' => col = col.saturating_sub(1),
            '\t' => {
                put(&mut line, &mut col, '\t');
            }
            c if c.is_control() => {}
            c => put(&mut line, &mut col, c),
        }
    }
    out.extend(line.iter());
    out
}

fn put(line: &mut Vec<char>, col: &mut usize, c: char) {
    if *col < line.len() {
        line[*col] = c;
    } else {
        line.push(c);
    }
    *col += 1;
}

/// The last `max_lines` lines of `text`, within `max_bytes`; `true` when
/// something was cut. Trailing blank lines are dropped first.
pub(super) fn tail_lines(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = text.trim_end().lines().map(str::trim_end).collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut cut = start > 0;
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines[start..].iter().rev() {
        if bytes + line.len() + 1 > max_bytes {
            cut = true;
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    (kept.join("\n"), cut)
}

/// Used by tests to wait for a terminal's exit.
#[cfg(test)]
pub(super) fn exit_of(manager: &SessionManager, terminal_id: &str) -> Option<TerminalExitStatus> {
    manager
        .terminals()
        .get(terminal_id)
        .ok()
        .and_then(|t| *lock(&t.exit))
}
