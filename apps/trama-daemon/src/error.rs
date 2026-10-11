use thiserror::Error;

pub type Result<T> = std::result::Result<T, DaemonError>;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

impl DaemonError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Protocol(_) => "protocol",
            Self::Json(_) => "invalid_json",
            Self::NotFound(_) => "not_found",
            Self::Invalid(_) => "invalid",
        }
    }
}
