use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::json;

/// Structured error body returned for all error responses.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// Application-level API error.
#[derive(Debug)]
pub enum ApiError {
    /// Malformed request (e.g. invalid JSON body).
    MalformedRequest(String),
    /// Generic bad request.
    BadRequest(String),
    /// Resource not found.
    NotFound(String),
    /// Internal server error.
    Internal(String),
}

impl ApiError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::MalformedRequest(_) => (StatusCode::BAD_REQUEST, "MALFORMED_REQUEST"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        }
    }

    fn message(&self) -> &str {
        match self {
            ApiError::MalformedRequest(m)
            | ApiError::BadRequest(m)
            | ApiError::NotFound(m)
            | ApiError::Internal(m) => m,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let body = ErrorBody {
            error: ErrorDetail {
                code: code.to_string(),
                message: self.message().to_string(),
            },
        };
        (status, Json(body)).into_response()
    }
}

/// Map axum extractor rejections (notably JSON deserialization failures)
/// into structured API errors so malformed bodies return 400, not 500.
impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(rejection: axum::extract::rejection::JsonRejection) -> Self {
        ApiError::MalformedRequest(rejection.body_text())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        ApiError::MalformedRequest(err.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err.to_string())
    }
}

/// Helper to build a JSON error response directly.
pub fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "code": code,
            "message": message,
        }
    });
    (status, Json(body)).into_response()
}
