//! Windows pseudo terminals via ConPTY (`portable-pty`), with the same
//! surface as the Unix `openpty` implementation in `pty.rs`.
//!
//! Differences: there are no process groups or signals, so `hang_up` and
//! `kill_group` both terminate the child process (ConPTY closes the
//! console for its descendants); exit statuses have a code and never a
//! signal.

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;

pub use portable_pty::ExitStatus;

#[derive(Debug, Clone)]
pub struct PtySpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub cols: u16,
    pub rows: u16,
}

pub struct PtyChild {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl PtyChild {
    pub fn spawn(spec: &PtySpec) -> io::Result<Self> {
        let pair = native_pty_system()
            .openpty(size(spec.cols, spec.rows))
            .map_err(other)?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        command.cwd(&spec.cwd);
        command.env_clear();
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let child = pair.slave.spawn_command(command).map_err(other)?;
        drop(pair.slave);
        let writer = pair.master.take_writer().map_err(other)?;
        Ok(Self {
            master: pair.master,
            writer: Mutex::new(Some(writer)),
            child,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.process_id().unwrap_or(0)
    }

    pub fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().map_err(other)
    }

    /// The keystroke writer. ConPTY has a single writer, so this succeeds
    /// once per terminal.
    pub fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        self.writer
            .lock()
            .map_err(other)?
            .take()
            .ok_or_else(|| other("terminal writer already taken"))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master.resize(size(cols, rows)).map_err(other)
    }

    pub fn hang_up(&mut self) {
        let _ = self.child.kill();
    }

    pub fn kill_group(&mut self) {
        let _ = self.child.kill();
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

/// `(code, signal)` of an exit status; Windows has no signals.
pub fn exit_parts(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (i32::try_from(status.exit_code()).ok(), None)
}
