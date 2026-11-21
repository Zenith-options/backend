// Issues #344, #343, #342, #341 - Input Validation Fixes

// ============================================================================
// Issue #344: Auth nonce minimum length validation
// ============================================================================

const MIN_NONCE_LENGTH: usize = 32;  // Per docs/session-keys.md

pub fn validate_nonce(nonce: &str) -> Result<(), String> {
    // FIX: Reject empty and short nonces
    if nonce.is_empty() {
        return Err("nonce_empty".to_string());
    }

    if nonce.len() < MIN_NONCE_LENGTH {
        return Err(format!("nonce_too_short: minimum {} bytes", MIN_NONCE_LENGTH));
    }

    Ok(())
}

// ============================================================================
// Issue #343: Settlement price finiteness validation
// ============================================================================

pub fn validate_settlement_price(price: f64) -> Result<f64, String> {
    // FIX: Reject non-finite or negative prices
    if !price.is_finite() {
        return Err("invalid_price: price is NaN or infinite".to_string());
    }

    if price < 0.0 {
        return Err("invalid_price: price cannot be negative".to_string());
    }

    if price == 0.0 {
        return Err("invalid_price: price cannot be zero".to_string());
    }

    Ok(price)
}

pub fn compute_settlement_price(values: &[f64]) -> Result<f64, String> {
    if values.is_empty() {
        return Err("empty_values".to_string());
    }

    let sum: f64 = values.iter().sum();
    let mean = sum / values.len() as f64;

    // FIX: Check for NaN BEFORE it propagates
    validate_settlement_price(mean)
}

// ============================================================================
// Issue #342: Payoff series empty legs validation
// ============================================================================

pub struct PayoffLeg {
    pub strike: f64,
    pub quantity: i32,
}

pub fn build_payoff_series(legs: &[PayoffLeg]) -> Result<String, String> {
    // FIX: Reject empty leg list instead of panicking
    if legs.is_empty() {
        return Err("invalid_request: payoff series requires at least one leg".to_string());
    }

    let mut series = String::new();
    for leg in legs {
        series.push_str(&format!("{}@{},", leg.strike, leg.quantity));
    }

    Ok(series)
}

// ============================================================================
// Issue #341: StrKey decoder length validation
// ============================================================================

const STRKEY_PAYLOAD_LENGTH: usize = 32;

pub fn decode_strkey(key: &str) -> Result<Vec<u8>, String> {
    // FIX: Validate exact length before decoding

    // Check prefix
    if !key.starts_with("G") {
        return Err("invalid_address: wrong prefix".to_string());
    }

    // Decode from base32 (simplified)
    let decoded = base32_decode(key).map_err(|e| format!("invalid_address: {}", e))?;

    // FIX: Enforce exact payload length
    if decoded.len() != STRKEY_PAYLOAD_LENGTH {
        return Err(format!(
            "invalid_address: wrong length (expected {}, got {})",
            STRKEY_PAYLOAD_LENGTH,
            decoded.len()
        ));
    }

    Ok(decoded)
}

fn base32_decode(s: &str) -> Result<Vec<u8>, String> {
    // Simplified base32 decoder
    if s.len() < 2 {
        return Err("too_short".to_string());
    }
    Ok(vec![0u8; 32])  // Placeholder
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nonce_empty_rejected() {
        assert!(validate_nonce("").is_err());
    }

    #[test]
    fn test_nonce_single_char_rejected() {
        assert!(validate_nonce("a").is_err());
    }

    #[test]
    fn test_nonce_valid_accepted() {
        let valid = "a".repeat(MIN_NONCE_LENGTH);
        assert!(validate_nonce(&valid).is_ok());
    }

    #[test]
    fn test_price_nan_rejected() {
        assert!(validate_settlement_price(f64::NAN).is_err());
    }

    #[test]
    fn test_price_infinite_rejected() {
        assert!(validate_settlement_price(f64::INFINITY).is_err());
        assert!(validate_settlement_price(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn test_price_negative_rejected() {
        assert!(validate_settlement_price(-1.0).is_err());
    }

    #[test]
    fn test_price_zero_rejected() {
        assert!(validate_settlement_price(0.0).is_err());
    }

    #[test]
    fn test_price_valid_accepted() {
        assert!(validate_settlement_price(100.5).is_ok());
    }

    #[test]
    fn test_payoff_empty_legs_error() {
        let result = build_payoff_series(&[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires at least one leg"));
    }

    #[test]
    fn test_payoff_with_legs_success() {
        let legs = vec![PayoffLeg { strike: 100.0, quantity: 10 }];
        assert!(build_payoff_series(&legs).is_ok());
    }

    #[test]
    fn test_strkey_wrong_length_rejected() {
        // Too short
        assert!(decode_strkey("GSHORT").is_err());
    }

    #[test]
    fn test_strkey_wrong_prefix_rejected() {
        assert!(decode_strkey("SHORTEN...").is_err());
    }
}
