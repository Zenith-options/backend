pub mod error_codes;

pub use error_codes::ErrorCode;

/// Builds the standard error envelope used across the API.
///
/// Accepts a typed [`ErrorCode`] so that codes cannot drift from
/// `docs/error-codes.json`; a typo becomes a compile error.
pub fn error_response(code: ErrorCode, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "code": code.as_str(),
            "message": message.into(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every code in `docs/error-codes.json` must have a variant, and vice versa.
    #[test]
    fn catalogue_matches_enum() {
        let raw = include_str!("../../docs/error-codes.json");
        let entries: Vec<serde_json::Value> = serde_json::from_str(raw).expect("valid catalogue");

        let catalogue: HashSet<&str> = entries
            .iter()
            .map(|e| e["code"].as_str().expect("code is a string"))
            .collect();
        let variants: HashSet<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();

        assert_eq!(catalogue, variants, "error catalogue and ErrorCode drifted");
    }
}
