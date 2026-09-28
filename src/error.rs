use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::de::DeserializeOwned;
use serde_json::json;
use validator::Validate;

/// A JSON-bodied error instead of the empty-body `StatusCode` rejections
/// every handler was returning — a client currently has to infer "why"
/// from the status code alone (was that 400 a bad option_type or a
/// non-positive strike?). Existing handlers keep working unchanged since
/// this converts `From<StatusCode>`; converting them to attach a real
/// message is a per-module follow-up, not required to introduce the type.
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl From<StatusCode> for AppError {
    fn from(status: StatusCode) -> Self {
        let message = status.canonical_reason().unwrap_or("error").to_string();
        Self {
            status,
            message,
            details: None,
        }
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
        let mut body = json!({ "error": self.message });
        if let Some(details) = self.details {
            body["details"] = details;
        }
        (self.status, Json(body)).into_response()
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

pub struct ValidatedQuery<T>(pub T);

#[axum::async_trait]
impl<S, T> FromRequestParts<S> for ValidatedQuery<T>
where
    T: DeserializeOwned + Validate + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(value) = Query::<T>::from_request_parts(parts, state).await?;
        validate_request(&value)?;
        Ok(ValidatedQuery(value))
    }
}

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

pub struct ValidatedJson<T>(pub T);

#[axum::async_trait]
impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned + Validate + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state).await?;
        validate_request(&value)?;
        Ok(ValidatedJson(value))
    }
}

fn validate_request<T: Validate>(value: &T) -> Result<(), AppError> {
    value.validate().map_err(|errors| {
        let fields: Vec<serde_json::Value> = errors
            .field_errors()
            .into_iter()
            .map(|(field, violations)| {
                let messages: Vec<String> = violations
                    .iter()
                    .map(|violation| {
                        violation
                            .message
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| format!("{} constraint failed", violation.code))
                    })
                    .collect();
                json!({ "field": field, "errors": messages })
            })
            .collect();
        AppError::new(StatusCode::UNPROCESSABLE_ENTITY, "request validation failed")
            .with_details(json!({ "fields": fields }))
    })
}

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
