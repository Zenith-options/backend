use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

/// A JSON-bodied error instead of the empty-body `StatusCode` rejections
/// every handler was returning — a client currently has to infer "why"
/// from the status code alone (was that 400 a bad option_type or a
/// non-positive strike?). Existing handlers keep working unchanged since
/// this converts `From<StatusCode>`; converting them to attach a real
/// message is a per-module follow-up, not required to introduce the type.
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<StatusCode> for AppError {
    fn from(status: StatusCode) -> Self {
        let message = status.canonical_reason().unwrap_or("error").to_string();
        Self { status, message }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
