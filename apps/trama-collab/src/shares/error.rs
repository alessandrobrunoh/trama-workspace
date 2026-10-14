//! Errors of the `/v3` routes. Messages never echo inputs.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::AuthError;

#[derive(Debug)]
pub(crate) enum ShareError {
    Auth(AuthError),
    NotFound,
    Forbidden(&'static str),
    Invalid(&'static str),
    /// Rejected event; the reason comes from shape/replay rules (no input).
    Rejected(String),
    Conflict(&'static str),
    Database,
}

impl From<AuthError> for ShareError {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}

impl From<rusqlite::Error> for ShareError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}

impl IntoResponse for ShareError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Auth(error) => return error.into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            Self::Forbidden(why) => (StatusCode::FORBIDDEN, why.to_owned()),
            Self::Invalid(why) => (StatusCode::BAD_REQUEST, why.to_owned()),
            Self::Rejected(why) => (StatusCode::BAD_REQUEST, why),
            Self::Conflict(why) => (StatusCode::CONFLICT, why.to_owned()),
            Self::Database => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            ),
        };
        (status, Json(json!({ "error": error }))).into_response()
    }
}
