//! Local, same-user IPC between the UI, helper processes and the daemon.
//!
//! The daemon is addressed by a filesystem path on every platform (the
//! historical socket path). On Unix that path is a Unix domain socket with
//! mode `0600`. On Windows it is mapped deterministically to a local named
//! pipe (`\\.\pipe\trama-<hash>`) that rejects remote clients; the path
//! itself is never created. Callers only see [`IpcListener`], [`IpcStream`]
//! and [`connect`], so no `cfg` leaks into the daemon or the desktop app.
//!
//! This is a local transport, not authentication: the IPC protocol's own
//! tokens and checks still apply.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(unix)]
mod imp {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    pub type Stream = UnixStream;

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            use std::os::unix::fs::PermissionsExt;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            let listener = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self(listener))
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            Ok(self.0.accept().await?.0)
        }
    }

    pub async fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path).await
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// `ERROR_PIPE_BUSY`: every server instance is currently connected.
    const ERROR_PIPE_BUSY: i32 = 231;

    pub enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            let name = super::pipe_name(path);
            // `first_pipe_instance` fails if another process already owns
            // the name, so a second daemon cannot hijack the endpoint.
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            Ok(Self { name, next })
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            self.next.connect().await?;
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            let connected = std::mem::replace(&mut self.next, fresh);
            Ok(Stream::Server(connected))
        }
    }

    pub async fn connect(path: &Path) -> io::Result<Stream> {
        let name = super::pipe_name(path);
        loop {
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(Stream::Client(client)),
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    macro_rules! delegate {
        ($self:ident, $s:ident => $e:expr) => {
            match $self.get_mut() {
                Stream::Server($s) => $e,
                Stream::Client($s) => $e,
            }
        };
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            delegate!(self, s => Pin::new(s).poll_read(cx, buf))
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            delegate!(self, s => Pin::new(s).poll_write(cx, buf))
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            delegate!(self, s => Pin::new(s).poll_flush(cx))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            delegate!(self, s => Pin::new(s).poll_shutdown(cx))
        }
    }
}

/// Named-pipe name for a daemon path. Stable across processes (FNV-1a over
/// the path's UTF-8/lossy bytes), so every client derives the same name.
pub fn pipe_name(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!(r"\\.\pipe\trama-{hash:016x}")
}

/// A connected, bidirectional local IPC stream.
pub struct IpcStream(imp::Stream);

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// Listening endpoint of the daemon.
pub struct IpcListener(imp::Listener);

impl IpcListener {
    /// Bind the endpoint for `path`, replacing a stale Unix socket file.
    pub fn bind(path: &Path) -> io::Result<Self> {
        imp::Listener::bind(path).map(Self)
    }

    /// Wait for the next client. Cancel-safe enough for `select!` loops:
    /// a dropped accept leaves the endpoint listening.
    pub async fn accept(&mut self) -> io::Result<IpcStream> {
        self.0.accept().await.map(IpcStream)
    }
}

/// Connect to the daemon endpoint for `path`.
pub async fn connect(path: &Path) -> io::Result<IpcStream> {
    imp::connect(path).await.map(IpcStream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn pipe_names_are_stable_and_distinct() {
        let a = pipe_name(Path::new("/tmp/a/daemon.sock"));
        assert_eq!(a, pipe_name(Path::new("/tmp/a/daemon.sock")));
        assert_ne!(a, pipe_name(Path::new("/tmp/b/daemon.sock")));
        assert!(a.starts_with(r"\\.\pipe\trama-"));
    }

    #[tokio::test]
    async fn round_trip_with_two_clients() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let mut listener = IpcListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let stream = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let (read, mut write) = tokio::io::split(stream);
                    let mut line = String::new();
                    BufReader::new(read).read_line(&mut line).await.unwrap();
                    write.write_all(format!("echo {line}").as_bytes()).await.unwrap();
                });
            }
        });
        for name in ["one", "two"] {
            let stream = connect(&path).await.unwrap();
            let (read, mut write) = tokio::io::split(stream);
            write.write_all(format!("{name}\n").as_bytes()).await.unwrap();
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            assert_eq!(line, format!("echo {name}\n"));
        }
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ipc.sock");
        let _listener = IpcListener::bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Rebinding replaces a stale socket file.
        drop(_listener);
        assert!(IpcListener::bind(&path).is_ok());
    }
}
