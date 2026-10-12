use thiserror::Error;
use trama_domain::DomainError;

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
    #[error("thread is busy")]
    Busy,
    #[error("thread is busy; wait for the running turn or migration to finish before migrating")]
    MigrationBusy,
    #[error("agent error: {0}")]
    Agent(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("collab: {0}")]
    Collab(String),
}

impl DaemonError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Protocol(_) => "protocol",
            Self::Json(_) => "invalid_json",
            Self::NotFound(_) => "not_found",
            Self::Invalid(_) => "invalid",
            Self::Busy | Self::MigrationBusy => "busy",
            Self::Agent(_) => "agent",
            Self::Storage(_) => "storage",
            Self::Collab(_) => "collab",
        }
    }
}

impl From<DomainError> for DaemonError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound(msg) => Self::NotFound(msg),
            DomainError::Invalid(msg) => Self::Invalid(msg),
            DomainError::Json(err) => Self::Json(err),
            DomainError::Io(err) => Self::Io(err),
            DomainError::Agent(msg) => Self::Agent(msg),
            DomainError::Storage(msg) => Self::Storage(msg),
            DomainError::Runtime(msg) => Self::Agent(msg),
        }
    }
}
