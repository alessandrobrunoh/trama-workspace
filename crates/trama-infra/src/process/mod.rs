//! Child-process helpers: pseudo-terminals and setup scripts.

#[cfg(unix)]
pub mod pty;
#[cfg(windows)]
#[path = "pty_windows.rs"]
pub mod pty;
pub mod setup;
