//! Pseudo-terminals for inline composer terminals.
//!
//! Built on `libc::openpty` (already a dependency) rather than a PTY crate:
//! macOS only needs `openpty`, `setsid`, `TIOCSCTTY` and `TIOCSWINSZ`, and
//! this keeps the dependency tree (and disk use) unchanged. The child gets
//! the slave as stdin/stdout/stderr and becomes a session leader with it as
//! its controlling terminal, so job control, Ctrl-C and resize (SIGWINCH)
//! work. Every unsafe call is a plain syscall on descriptors owned here.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

/// What to run in a new PTY. The environment is exactly `env`; nothing is
/// inherited implicitly.
#[derive(Debug, Clone)]
pub struct PtySpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub cols: u16,
    pub rows: u16,
}

/// A process running on a PTY. Read output from [`PtyChild::reader`], write
/// keystrokes to [`PtyChild::writer`]. Dropping it closes the master, which
/// hangs up the terminal (SIGHUP to its session).
pub struct PtyChild {
    master: OwnedFd,
    child: Child,
}

impl PtyChild {
    pub fn spawn(spec: &PtySpec) -> io::Result<Self> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut size = winsize(spec.cols, spec.rows);
        // SAFETY: out-pointers are valid; name and termios are optional.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty returned two fresh descriptors we now own.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        set_cloexec(&master)?;
        set_cloexec(&slave)?;

        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        // `command` (and its slave copies) is dropped here, so only the child
        // holds the slave: reads end once it and its descendants exit.
        drop(command);
        Ok(Self { master, child })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// A blocking reader of the terminal's output. Returns EOF (or EIO) once
    /// every process holding the slave has exited.
    pub fn reader(&self) -> io::Result<File> {
        Ok(File::from(self.master.try_clone()?))
    }

    /// A blocking writer for keystrokes.
    pub fn writer(&self) -> io::Result<File> {
        Ok(File::from(self.master.try_clone()?))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let size = winsize(cols, rows);
        // SAFETY: TIOCSWINSZ reads a winsize from a valid pointer.
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Send `signal` to the terminal's whole process group (the child is a
    /// session and group leader).
    pub fn signal_group(&self, signal: libc::c_int) {
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            // SAFETY: plain syscall; a negative pid targets the group.
            unsafe {
                libc::kill(-pid, signal);
            }
        }
    }

    /// SIGHUP to the terminal's process group, as when a terminal closes.
    pub fn hang_up(&self) {
        self.signal_group(libc::SIGHUP);
    }

    /// SIGKILL to the terminal's process group.
    pub fn kill_group(&self) {
        self.signal_group(libc::SIGKILL);
    }

    /// Non-blocking exit check.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

/// `(code, signal)` of an exit status.
pub fn exit_parts(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), status.signal())
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn spec(args: &[&str]) -> PtySpec {
        PtySpec {
            program: "/bin/sh".into(),
            args: args.iter().map(OsString::from).collect(),
            cwd: std::env::temp_dir(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            cols: 80,
            rows: 24,
        }
    }

    fn read_all(mut reader: File) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break, // EIO once the slave is gone
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn runs_on_a_tty_with_the_requested_size() {
        let mut child =
            PtyChild::spawn(&spec(&["-c", "test -t 1 && echo tty; stty size"])).unwrap();
        let reader = child.reader().unwrap();
        let output = std::thread::spawn(move || read_all(reader));
        let status = child.wait().unwrap();
        drop(child);
        let output = output.join().unwrap();
        assert!(status.success());
        assert!(output.contains("tty"), "{output:?}");
        assert!(output.contains("24 80"), "{output:?}");
    }

    #[test]
    fn echoes_input_and_resizes() {
        let mut child =
            PtyChild::spawn(&spec(&["-c", "read line; echo got:$line; stty size"])).unwrap();
        let reader = child.reader().unwrap();
        let output = std::thread::spawn(move || read_all(reader));
        child.resize(100, 30).unwrap();
        child.writer().unwrap().write_all(b"hello\n").unwrap();
        child.wait().unwrap();
        drop(child);
        let output = output.join().unwrap();
        assert!(output.contains("got:hello"), "{output:?}");
        assert!(output.contains("30 100"), "{output:?}");
    }

    #[test]
    fn group_signal_kills_the_terminal() {
        let mut child = PtyChild::spawn(&spec(&["-c", "sleep 30"])).unwrap();
        child.signal_group(libc::SIGKILL);
        let status = child.wait().unwrap();
        assert_eq!(exit_parts(status), (None, Some(libc::SIGKILL)));
    }
}
