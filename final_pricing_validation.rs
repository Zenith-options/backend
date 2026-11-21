// Issues #340, #339, #338, #337 - Pricing and Validation

use std::f64::NAN;

// ============================================================================
// Issue #340: Black-Scholes at expiry (t=0)
// ============================================================================

pub fn black_scholes(S: f64, K: f64, r: f64, sigma: f64, T: f64, is_call: bool) -> Result<f64, String> {
    // FIX: Guard expiry case (T = 0)
    if (T - 0.0).abs() < 1e-10 {
        // At expiry, return intrinsic value
        if is_call {
            return Ok((S - K).max(0.0));  // max(S-K, 0)
        } else {
            return Ok((K - S).max(0.0));  // max(K-S, 0)
        }
    }

    if sigma <= 0.0 || T < 0.0 {
        return Err("Invalid parameters".into());
    }

    // Normal Black-Scholes calculation
    let d1 = ((S / K).ln() + (r + 0.5 * sigma * sigma) * T) / (sigma * T.sqrt());
    let d2 = d1 - sigma * T.sqrt();

    let price = if is_call {
        S * normal_cdf(d1) - K * (-r * T).exp() * normal_cdf(d2)
    } else {
        K * (-r * T).exp() * normal_cdf(-d2) - S * normal_cdf(-d1)
    };

    Ok(price)
}

fn normal_cdf(x: f64) -> f64 {
    (1.0 + libm::erf(x / 2.0_f64.sqrt())) / 2.0
}

// ============================================================================
// Issue #339: Implied volatility solver lower bound check
// ============================================================================

pub fn solve_implied_vol(market_price: f64, S: f64, K: f64, r: f64, T: f64, is_call: bool) -> Result<f64, String> {
    // FIX: Check if market price is below intrinsic (impossible scenario)
    let intrinsic = if is_call {
        (S - K).max(0.0)
    } else {
        (K - S).max(0.0)
    };

    if market_price < intrinsic {
        return Err("Price below intrinsic value".into());
    }

    // Bisection solver
    let mut vol_low = 0.001;
    let mut vol_high = 5.0;

    for _ in 0..100 {
        let vol_mid = (vol_low + vol_high) / 2.0;

        let price = black_scholes(S, K, r, vol_mid, T, is_call)?;

        if (price - market_price).abs() < 1e-6 {
            return Ok(vol_mid);
        }

        if price < market_price {
            vol_low = vol_mid;
        } else {
            vol_high = vol_mid;
        }
    }

    Err("Failed to converge".into())
}

// ============================================================================
// Issue #338: Cursor pagination rejects negative offsets
// ============================================================================

pub struct CursorQuery {
    pub limit: i32,
    pub offset: i32,
}

pub fn validate_cursor_query(limit: i32, offset: i32) -> Result<CursorQuery, String> {
    const MAX_LIMIT: i32 = 1000;

    // FIX: Reject negative values instead of clamping
    if limit < 1 {
        return Err("Limit must be positive".into());
    }

    if offset < 0 {
        return Err("Offset cannot be negative".into());
    }

    // Clamp to max (after validation)
    let clamped_limit = limit.min(MAX_LIMIT);

    Ok(CursorQuery {
        limit: clamped_limit,
        offset,
    })
}

// ============================================================================
// Issue #337: Reject zero or negative strike and expiry
// ============================================================================

pub struct OptionPosition {
    pub strike: f64,
    pub expiry: u64,
}

pub fn validate_option_position(strike: f64, expiry: u64, now: u64) -> Result<OptionPosition, String> {
    // FIX: Reject zero or negative strike
    if strike <= 0.0 {
        return Err("Strike must be strictly positive".into());
    }

    // FIX: Reject non-positive expiry
    if expiry <= now {
        return Err("Expiry must be in the future".into());
    }

    Ok(OptionPosition { strike, expiry })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bs_at_expiry_call() {
        let price = black_scholes(100.0, 90.0, 0.05, 0.2, 0.0, true).unwrap();
        assert!((price - 10.0).abs() < 1e-6);  // Intrinsic = 10
    }

    #[test]
    fn test_bs_at_expiry_put() {
        let price = black_scholes(90.0, 100.0, 0.05, 0.2, 0.0, false).unwrap();
        assert!((price - 10.0).abs() < 1e-6);  // Intrinsic = 10
    }

    #[test]
    fn test_iv_below_intrinsic_rejected() {
        assert!(solve_implied_vol(5.0, 100.0, 90.0, 0.05, 1.0, true).is_err());
    }

    #[test]
    fn test_negative_offset_rejected() {
        assert!(validate_cursor_query(10, -1).is_err());
    }

    #[test]
    fn test_zero_strike_rejected() {
        assert!(validate_option_position(0.0, 1000, 500).is_err());
    }

    #[test]
    fn test_expired_option_rejected() {
        assert!(validate_option_position(100.0, 500, 500).is_err());
    }
}
