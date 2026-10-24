//! Exact integer amounts for anything that touches on-chain value.
//!
//! The paper-trading tables store money as `REAL`. Stellar amounts are
//! integers in stroops (10⁻⁷ of a unit). Code that compares off-chain
//! balances with on-chain balances (the cutover reconciliation, settlement
//! previews) converts once, at the boundary, with [`to_stroops`], and then
//! works in `i64` so equality is exact.

pub const STROOPS_PER_UNIT: i64 = 10_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum AmountError {
    NotFinite(f64),
    OutOfRange(f64),
}

impl std::fmt::Display for AmountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFinite(v) => write!(f, "amount {v} is not finite"),
            Self::OutOfRange(v) => write!(f, "amount {v} does not fit in i64 stroops"),
        }
    }
}

impl std::error::Error for AmountError {}

/// Converts a `REAL` amount to stroops, rounding half away from zero (the
/// same rule as `migrations/0007_money_fixed_point.sql`). NaN, ±∞ and
/// values outside the `i64` range are errors, never silently clamped.
pub fn to_stroops(value: f64) -> Result<i64, AmountError> {
    if !value.is_finite() {
        return Err(AmountError::NotFinite(value));
    }
    let scaled = (value * STROOPS_PER_UNIT as f64).round();
    // i64::MAX as f64 rounds up to 2^63, so `>=` excludes it.
    if scaled >= i64::MAX as f64 || scaled < i64::MIN as f64 {
        return Err(AmountError::OutOfRange(value));
    }
    Ok(scaled as i64)
}

/// Display-only conversion back to units.
pub fn from_stroops(stroops: i64) -> f64 {
    stroops as f64 / STROOPS_PER_UNIT as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(to_stroops(1.0).unwrap(), 10_000_000);
        assert_eq!(to_stroops(0.000_000_05).unwrap(), 1);
        assert_eq!(to_stroops(-0.000_000_05).unwrap(), -1);
        assert_eq!(to_stroops(100_000.0).unwrap(), 1_000_000_000_000);
        assert_eq!(to_stroops(0.1 + 0.2).unwrap(), 3_000_000);
    }

    #[test]
    fn rejects_non_finite_and_overflow() {
        assert!(to_stroops(f64::NAN).is_err());
        assert!(to_stroops(f64::INFINITY).is_err());
        assert!(to_stroops(1e13).is_err());
        assert!((from_stroops(12_345_678) - 1.234_567_8).abs() < 1e-12);
    }
}
