use axum::http::{Request, StatusCode};
use tower_governor::{key_extractor::KeyExtractor, GovernorError};

#[derive(Clone, Copy, Debug, Default)]
pub struct BearerOrIpKeyExtractor;

impl KeyExtractor for BearerOrIpKeyExtractor {
    type Key = String;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        if let Some(auth) = req.headers().get("authorization") {
            if let Ok(value) = auth.to_str() {
                if let Some(token) = value.strip_prefix("Bearer ") {
                    let token = token.trim();
                    if !token.is_empty() {
                        return Ok(format!("bearer:{token}"));
                    }
                }
            }
        }

        if let Some(ip) = req
            .headers()
            .get("x-forwarded-for")
            .or_else(|| req.headers().get("x-real-ip"))
        {
            if let Ok(value) = ip.to_str() {
                let ip = value.split(',').next().unwrap_or(value).trim();
                if !ip.is_empty() {
                    return Ok(format!("ip:{ip}"));
                }
            }
        }

        if let Some(addr) = req.extensions().get::<std::net::SocketAddr>() {
            return Ok(format!("ip:{}", addr.ip()));
        }

        Ok("anonymous".to_string())
    }
}
