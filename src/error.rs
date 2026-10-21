use axum::extract::{FromRequest, FromRequestParts, Query};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone)]
pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    pub fn BadRequest(message: impl Into<String>) -> Self {
        Self::bad_request(message)
    }

    pub fn Internal(message: impl Into<String>) -> Self {
        Self::internal(message)
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

pub type ApiError = AppError;

pub fn db_error(context: &str, err: impl std::fmt::Display) -> AppError {
    AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{context}: {err}"))
}

pub struct AppJson<T>(pub T);

#[async_trait::async_trait]
impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send + 'static,
{
    type Rejection = AppError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
        Ok(Self(value))
    }
}

pub struct AppQuery<T>(pub T);

#[async_trait::async_trait]
impl<S, T> FromRequestParts<S> for AppQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send + 'static,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let query = Query::<T>::from_request_parts(parts, state)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
        Ok(Self(query.0))
    }
}

pub struct AuthUser(pub String);

#[async_trait::async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(header) = parts.headers.get("authorization") {
            let value = header.to_str().unwrap_or_default();
            if let Some(rest) = value.strip_prefix("Bearer ") {
                let trimmed = rest.trim();
                if !trimmed.is_empty() {
                    return Ok(Self(trimmed.to_string()));
                }
            }
        }

        if let Some(header) = parts.headers.get("x-wallet") {
            let value = header.to_str().unwrap_or_default().trim();
            if !value.is_empty() {
                return Ok(Self(value.to_string()));
            }
        }

        if let Some(query) = parts.uri.query() {
            let parsed = url::form_urlencoded::parse(query.as_bytes());
            for (key, value) in parsed {
                if key == "wallet" {
                    let wallet = value.trim();
                    if !wallet.is_empty() {
                        return Ok(Self(wallet.to_string()));
                    }
                }
            }
        }

        Err(AppError::new(StatusCode::UNAUTHORIZED, "missing or invalid auth token"))
    }
}
