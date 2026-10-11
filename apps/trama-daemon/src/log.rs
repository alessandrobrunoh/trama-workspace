//! Minimal stderr logger — avoids pulling `tracing` into the skeleton.

use std::fmt::Display;

pub fn info(message: impl Display) {
    eprintln!("[trama-daemon] {message}");
}

pub fn warn(message: impl Display) {
    eprintln!("[trama-daemon] warn: {message}");
}

pub fn error(message: impl Display) {
    eprintln!("[trama-daemon] error: {message}");
}
