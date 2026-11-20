use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

// Carry structured failures through challenge/range responses without reading bodies.
#[derive(Clone)]
pub(crate) struct ErrorDetails {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct AppError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}
pub type Result<T> = std::result::Result<T, AppError>;
impl AppError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }
    pub fn missing(code: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, "content not found")
    }
    pub fn denied() -> Self {
        Self::new(StatusCode::FORBIDDEN, "DENIED", "permission denied")
    }
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "authentication required",
        )
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "UNKNOWN", message)
    }
    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "operation failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "storage or internal operation failed",
        )
    }
}
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::internal(e)
    }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::internal(e)
    }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let details = ErrorDetails {
            code: self.code,
            message: self.message.clone(),
        };
        let mut response = (
            self.status,
            Json(json!({"errors":[{"code":self.code,"message":self.message}]})),
        )
            .into_response();
        response.extensions_mut().insert(details);
        response
    }
}
