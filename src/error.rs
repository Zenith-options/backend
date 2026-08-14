use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::de::DeserializeOwned;
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

impl From<QueryRejection> for AppError {
    fn from(rejection: QueryRejection) -> Self {
        AppError::new(rejection.status(), rejection.body_text())
    }
}

/// Drop-in replacement for `axum::extract::Query<T>` whose rejection is a
/// JSON `{"error": "..."}` body via AppError, instead of axum's default
/// plain-text rejection — the only place in this API a malformed request
/// broke the JSON-error-body contract every other handler upholds. Every
/// `Query<T>` extractor in the app should use this instead.
pub struct AppQuery<T>(pub T);

#[axum::async_trait]
impl<S, T> FromRequestParts<S> for AppQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(value) = Query::<T>::from_request_parts(parts, state).await?;
        Ok(AppQuery(value))
    }
}

impl From<JsonRejection> for AppError {
    fn from(rejection: JsonRejection) -> Self {
        AppError::new(rejection.status(), rejection.body_text())
    }
}

/// Drop-in replacement for `axum::extract::Json<T>` whose rejection is a
/// JSON `{"error": "..."}` body via AppError, instead of axum's default
/// plain-text rejection (e.g. "Failed to deserialize the JSON body into
/// the target type: ..."). Every `Json<T>` request-body extractor in the
/// app should use this instead.
pub struct AppJson<T>(pub T);

#[axum::async_trait]
impl<S, T> FromRequest<S> for AppJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state).await?;
        Ok(AppJson(value))
    }
}
