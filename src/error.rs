use axum::body::{to_bytes, Body};
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

tokio::task_local! {
    static REQUEST_CONTEXT: RequestContext;
}

#[derive(Clone)]
struct RequestContext {
    id: String,
    legacy_errors: bool,
}

/// API failures use a stable code and request correlation ID. V1 retains its
/// legacy body by default; v2 always uses the structured representation.
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub code: ErrorCode,
    pub details: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    ClientError,
    Conflict,
    Forbidden,
    InsufficientBalance,
    InternalError,
    InvalidCredentials,
    InvalidRequest,
    NotFound,
    RateLimited,
    ServiceUnavailable,
    Unauthenticated,
    UnprocessableEntity,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientError => "CLIENT_ERROR",
            Self::Conflict => "CONFLICT",
            Self::Forbidden => "FORBIDDEN",
            Self::InsufficientBalance => "INSUFFICIENT_BALANCE",
            Self::InternalError => "INTERNAL_ERROR",
            Self::InvalidCredentials => "INVALID_CREDENTIALS",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::NotFound => "NOT_FOUND",
            Self::RateLimited => "RATE_LIMITED",
            Self::ServiceUnavailable => "SERVICE_UNAVAILABLE",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::UnprocessableEntity => "UNPROCESSABLE_ENTITY",
        }
    }
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code_for(status),
            message: message.into(),
            details: json!({}),
        }
    }

    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = code;
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

impl From<StatusCode> for AppError {
    fn from(status: StatusCode) -> Self {
        let message = status.canonical_reason().unwrap_or("error").to_string();
        Self::new(status, message)
    }
}

/// Turns an unexpected sqlx::Error into a 500 whose client-facing message
/// names WHICH operation failed ("failed to load alerts") without leaking
/// the raw database error (table/column names, SQL fragments) into the
/// response body — the raw error still goes to the logs via tracing, for
/// whoever's actually debugging it.
pub fn db_error(context: &str, e: sqlx::Error) -> AppError {
    tracing::error!(error = %e, context, "database operation failed");
    AppError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("failed to {context}"),
    )
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let context = REQUEST_CONTEXT
            .try_with(Clone::clone)
            .unwrap_or_else(|_| RequestContext {
                id: uuid::Uuid::new_v4().to_string(),
                legacy_errors: false,
            });
        let body = if context.legacy_errors {
            json!({ "error": self.message })
        } else {
            json!({
                "error": {
                    "code": self.code.as_str(),
                    "message": self.message,
                    "details": self.details,
                    "request_id": context.id
                }
            })
        };
        let mut response = (self.status, Json(body)).into_response();
        if let Ok(value) = HeaderValue::from_str(&context.id) {
            response.headers_mut().insert("x-request-id", value);
        }
        response
    }
}

fn code_for(status: StatusCode) -> ErrorCode {
    match status {
        StatusCode::BAD_REQUEST => ErrorCode::InvalidRequest,
        StatusCode::UNAUTHORIZED => ErrorCode::Unauthenticated,
        StatusCode::FORBIDDEN => ErrorCode::Forbidden,
        StatusCode::NOT_FOUND => ErrorCode::NotFound,
        StatusCode::CONFLICT => ErrorCode::Conflict,
        StatusCode::TOO_MANY_REQUESTS => ErrorCode::RateLimited,
        StatusCode::UNPROCESSABLE_ENTITY => ErrorCode::UnprocessableEntity,
        StatusCode::SERVICE_UNAVAILABLE => ErrorCode::ServiceUnavailable,
        StatusCode::INTERNAL_SERVER_ERROR => ErrorCode::InternalError,
        _ if status.is_client_error() => ErrorCode::ClientError,
        _ => ErrorCode::InternalError,
    }
}

pub async fn request_context_middleware(mut request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        request.headers_mut().insert("x-request-id", value);
    }
    let path = request.uri().path();
    let forced_legacy = request
        .headers()
        .get("x-api-error-format")
        .and_then(|v| v.to_str().ok())
        == Some("legacy");
    let forced_structured = request
        .headers()
        .get("x-api-error-format")
        .and_then(|v| v.to_str().ok())
        == Some("structured");
    let legacy_errors = forced_legacy || (path.starts_with("/api/v1/") && !forced_structured);
    let context = RequestContext {
        id: request_id.clone(),
        legacy_errors,
    };
    let mut response = REQUEST_CONTEXT.scope(context, next.run(request)).await;

    if response.status().is_client_error() || response.status().is_server_error() {
        let status = response.status();
        let (parts, body) = response.into_parts();
        let bytes = match to_bytes(body, 1024 * 1024).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(%error, "failed to read error response body");
                return AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to build error response",
                )
                .into_response();
            }
        };
        let existing: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let has_error = existing.get("error").is_some();
        if has_error {
            response = Response::from_parts(parts, Body::from(bytes));
        } else {
            let message = String::from_utf8_lossy(&bytes).into_owned();
            let code = code_for(status).as_str();
            let body = if legacy_errors {
                json!({ "error": if message.is_empty() { status.canonical_reason().unwrap_or("error") } else { &message } })
            } else {
                json!({
                    "error": {
                        "code": code,
                        "message": if message.is_empty() { status.canonical_reason().unwrap_or("error") } else { &message },
                        "details": {},
                        "request_id": request_id
                    }
                })
            };
            response = (status, Json(body)).into_response();
            response.headers_mut().extend(parts.headers);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_error_names_the_operation_without_leaking_the_raw_sqlx_error() {
        let raw = sqlx::Error::RowNotFound;
        let app_err = db_error("load account", raw);

        assert_eq!(app_err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(app_err.message, "failed to load account");
        // The whole point: the client-facing message must not be a
        // passthrough of sqlx's own Display text ("no rows returned").
        assert!(!app_err.message.contains("row"));
    }

    #[test]
    fn app_error_from_status_code_uses_the_canonical_reason() {
        let app_err: AppError = StatusCode::NOT_FOUND.into();
        assert_eq!(app_err.message, "Not Found");
    }
}
