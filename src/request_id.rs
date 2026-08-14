use axum::http::{HeaderValue, Request};
use tower_http::request_id::{MakeRequestId, RequestId};

/// Generates a fresh UUIDv4 for every request that doesn't already carry
/// an `x-request-id` header (tower_http's SetRequestIdLayer only calls
/// this when one isn't already present, so a caller's own request ID
/// passes through untouched — useful for tracing a request across
/// services that each set their own ID by default).
#[derive(Clone, Default)]
pub struct MakeRequestUuid;

impl MakeRequestId for MakeRequestUuid {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        let id = uuid::Uuid::new_v4().to_string();
        HeaderValue::from_str(&id).ok().map(RequestId::new)
    }
}
