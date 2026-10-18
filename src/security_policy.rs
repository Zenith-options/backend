use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct HttpSecurityPolicy {
    allowed_origins: HashSet<String>,
    pub global_body_limit_bytes: usize,
    pub request_timeout_seconds: u64,
}

impl HttpSecurityPolicy {
    pub fn production(origins: impl IntoIterator<Item = String>, timeout_seconds: u64) -> Result<Self, String> {
        let allowed_origins: HashSet<_> = origins.into_iter().collect();
        if allowed_origins.is_empty() || allowed_origins.contains("*") {
            return Err("production CORS requires an explicit non-wildcard allowlist".into());
        }
        Ok(Self { allowed_origins, global_body_limit_bytes: 64 * 1024, request_timeout_seconds: timeout_seconds })
    }

    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins.contains(origin)
    }

    pub fn response_headers(&self) -> [(&'static str, &'static str); 5] {
        [
            ("strict-transport-security", "max-age=63072000; includeSubDomains"),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "strict-origin-when-cross-origin"),
            ("cross-origin-resource-policy", "same-site"),
            ("content-security-policy", "default-src 'none'; frame-ancestors 'none'"),
        ]
    }
}
