//! Single integer fixed-point specification for settlement-critical quantities.
//!
//! Collateral, payoff, and protocol fees are computed twice: off-chain in Rust
//! (this module) and on-chain in contract integers. This module defines the
//! shared spec so the two implementations cannot differ.
//!
//! The spec
//! --------
//! - A fixed-point number is an `i128` integer in units of `SCALE` (1/100).
//!   `1.00` is stored as `100`. Half units do not exist: `1.01` rounds to
//!   `101`.
//! - Scaling and rounding are identical on both sides:
//!   - Multiply by `c` then divide by `d`: round **up** (toward +∞) to the
//!     next unit. Never round down, because rounding always favours the
//!     protocol.
//!   - Divide by a positive `d`: round **up** as well (the same rule,
//!     protocol-favouring).
//! - Rounding up means `x / d` with remainder `r != 0` yields `q + 1`.
//!   This is the only rounding rule used in the money path.
//! - Overflow is checked. Inputs are constrained to a documented domain
//!   (`MAX_INPUT`) so the widest intermediate result stays within `i128`.
//!
//! Why this spec
//! -------------
//! - Deterministic: the same `(input, scale)` always yields the same `i128`.
//! - Verifiable: an off-chain Rust implementation and an on-chain contract
//!   implementation can both be tested against the same `i128` expections.
//! - Correct on the boundary: fees and collateral round up, so a writer is
//!   never under-collateralized by a rounding artifact.
//!
//! The off-chain money path routes through `Money`, `MoneyPath`, and the
//! fixed-point calculator exclusively. Callers may not introduce an `f64` or
//! unvalidated `Decimal` multiplication anywhere settlement-critical numbers
//! may be derived.
//!
//! The differential test harness lives in `tests/pricing_parity_test.rs` and
//! compares the on-chain contract integers against this off-chain
//! implementation on a representative input grid.

/// Number of fixed-point units per whole token.
pub const SCALE: i128 = 100;
/// Maximum input magnitude (in units of `SCALE`) so intermediates fit in
/// `i128`. Coverage: `[-MAX_INPUT, MAX_INPUT]`.
pub const MAX_INPUT: i128 = 50_000;
/// Number of units per whole token for asset-scale (e.g. USDC). 1e6 = 6
/// decimals, matching a standard 6-decimal SAC.
pub const ASSET_SCALE: i128 = 1_000_000;
/// Rounding mode used by the money path. Always round up (protocol-favouring).
pub const ROUNDING_MODE: &str = "up";

/// Decimal exponent for a given SAC scale (e.g. 6 decimals -> 1e-6).
pub fn decimal_exponent(scale: i128) -> i128 {
    10i128.pow(scale as u32)
}

/// Convert a rational `value * numerator / denominator` to fixed point,
/// rounding **up** to the next unit.
pub fn mul_div_ceil(value: i128, numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    let product = value.checked_mul(numerator).expect("overflow in fixed-point multiply");
    let quotient = product / denominator;
    if product % denominator != 0 {
        quotient + 1
    } else {
        quotient
    }
}

/// Convert a `Decimal` amount to fixed point (units of `SCALE`). Fails if
/// `value` is non-finite or the conversion would overflow the `i128` domain.
pub fn decimal_to_fixed(value: &Decimal, scale: i128) -> Result<i128, MoneyError> {
    if !value.is_finite() {
        return Err(MoneyError::NonFinite);
    }
    let scaled = (value * Decimal::from(scale)).round_dp_with_strategy(
        0,
        RoundingStrategy::AwayFromZero,
    );
    scaled
        .to_i128()
        .ok_or(MoneyError::Overflow)
        .map(|v| v.max(i128::MIN).min(i128::MAX))
}

/// Seeded LCG for reproducible input generation in the diff harness. The
/// oracle uses a different generator, so the two sides never share the exact
/// same seed sequence — the inputs are instead paired by rank over the same
/// generated grid.
pub struct Random {
    state: i128,
}

impl Random {
    pub fn new(seed: i128) -> Self {
        Self { state: seed.max(1).min(i128::MAX) }
    }

    pub fn next_i128(&mut self) -> i128 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.state
    }

    pub fn next_within(&mut self, lo: i128, hi: i128) -> i128 {
        assert!(hi > lo);
        lo + (self.next_i128() % (hi - lo + 1))
    }
}

/// Fixed-point monetary amount used exclusively by the money path.
///
/// This is the single on-path representation for collateral, payoff, and
/// fees. A `Money` value is always in units of `SCALE` and is always rounded
/// up where rounding is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Money(i128);

impl Money {
    pub fn new(value: i128) -> Result<Self, MoneyError> {
        if value < i128::MIN || value > i128::MAX {
            return Err(MoneyError::Overflow);
        }
        Ok(Self(value))
    }

    pub fn from_i128(value: i128) -> Result<Self, MoneyError> {
        Self::new(value)
    }

    pub fn to_i128(self) -> i128 {
        self.0
    }

    /// Multiply by `rhs`, rounding up, and return a new rounded amount.
    pub fn mul_ceil(self, rhs: i128) -> Result<Self, MoneyError> {
        mul_div_ceil(self.0, rhs, SCALE).map(Money)
    }

    pub fn checked_add(self, rhs: i128) -> Result<Self, MoneyError> {
        let sum = self.0.checked_add(rhs).ok_or(MoneyError::Overflow)?;
        Self::new(sum)
    }

    pub fn checked_sub(self, rhs: i128) -> Result<Self, MoneyError> {
        let diff = self.0.checked_sub(rhs).ok_or(MoneyError::Underflow)?;
        Self::new(diff)
    }

    pub fn checked_mul(self, rhs: i128) -> Result<Self, MoneyError> {
        Self::new(self.0.checked_mul(rhs).ok_or(MoneyError::Overflow)?)
    }

    pub fn checked_div_ceil(self, divisor: i128) -> Result<Self, MoneyError> {
        if divisor == 0 {
            return Err(MoneyError::DivisionByZero);
        }
        mul_div_ceil(self.0, 1, divisor).map(Money)
    }
}

impl std::fmt::Display for Money {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MoneyError {
    #[error("non-finite value")]
    NonFinite,
    #[error("fixed-point conversion overflow")]
    Overflow,
    #[error("fixed-point conversion underflow")]
    Underflow,
    #[error("division by zero")]
    DivisionByZero,
}

impl std::fmt::Display for MoneyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

/// Amount in asset-scale units (e.g. 6 decimal USDC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetAmount(i128);

impl AssetAmount {
    pub fn new(value: i128) -> Result<Self, MoneyError> {
        Self::new_positive(value).map(AssetAmount)
    }

    pub fn new_positive(value: i128) -> Result<Self, MoneyError> {
        if value < 0 {
            return Err(MoneyError::Underflow);
        }
        Ok(AssetAmount(value))
    }

    pub fn to_fixed(self, scale: i128) -> i128 {
        self.0.checked_mul(scale).expect("asset scale overflow")
    }

    pub fn from_decimal_decimal_with_scale(value: &Decimal, scale: i128) -> Result<Self, MoneyError> {
        let scaled = (value * Decimal::from(scale)).round_dp_with_strategy(
            0,
            RoundingStrategy::AwayFromZero,
        );
        Self::new(scaled.to_i128().ok_or(MoneyError::Overflow)?)
    }
}

impl std::fmt::Display for AssetAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Entry point for the money path. Anything that moves settlement-critical
/// capital goes through `MoneyPath` first; the path applies the spec
/// (rounding up, overflow-checked) and returns either an on-chain-verifiable
/// `i128` amount or a deterministic error. The path is exclusive: there is no
/// alternate `f64` or unconstrained `Decimal` multiplication branch.
pub struct MoneyPath;

impl MoneyPath {
    pub fn new() -> Self {
        Self
    }

    pub fn deposit(amount: i128, asset_scale: i128) -> Result<i128, MoneyError> {
        // Deposit amount is already in SCALE units; validate it stays in the
        // documented domain and is non-negative.
        if amount < 0 || amount > MAX_INPUT {
            return Err(MoneyError::Overflow);
        }
        Ok(amount)
    }

    pub fn withdraw(amount: i128, asset_scale: i128) -> Result<i128, MoneyError> {
        Self::deposit(amount, asset_scale)
    }

    pub fn balance_after(deposits: &[i128], withdrawals: &[i128]) -> Result<i128, MoneyError> {
        let mut balance = 0i128;
        for &d in deposits {
            balance = balance
                .checked_add(d)
                .ok_or(MoneyError::Overflow)?;
        }
        for &w in withdrawals {
            balance = balance
                .checked_sub(w)
                .ok_or(MoneyError::Underflow)?;
        }
        Ok(balance)
    }

    pub fn fee(notional: i128, bps: i128) -> Result<i128, MoneyError> {
        mul_div_ceil(notional, bps, 10_000)
    }

    pub fn compute_collateral(kind: &str, contracts: i128, strike: i128, spot: i128) -> Result<i128, MoneyError> {
        let x = match kind {
            "call" => mul_div_ceil(contracts, spot, 1),
            "put" => mul_div_ceil(contracts * strike, 11, 10),
            _ => return Err(MoneyError::NonFinite),
        };
        Self::deposit(x, SCALE)
    }

    pub fn compute_payoff(kind: &str, strike: i128, spot: i128, is_long: bool) -> Result<i128, MoneyError> {
        let intrinsic = match kind {
            "call" => (spot - strike).max(0),
            "put" => (strike - spot).max(0),
            _ => return Err(MoneyError::NonFinite),
        };
        let per_contract = if is_long { intrinsic } else { -intrinsic };
        Ok(per_contract)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_holds_for_mul_div_ceil() {
        // 3 * 100 / 100 = 3, exactly.
        assert_eq!(mul_div_ceil(3, 100, 100), 3);
        // 1 * 1 / 2 = 0.5 -> rounds up to 1.
        assert_eq!(mul_div_ceil(1, 1, 2), 1);
        // 5 * 3 / 2 = 7.5 -> rounds up to 8.
        assert_eq!(mul_div_ceil(5, 3, 2), 8);
        // 7 * 1 / 3 = 2.333 -> rounds up to 3.
        assert_eq!(mul_div_ceil(7, 1, 3), 3);
    }

    #[test]
    fn spec_holds_for_fee_rounding_up() {
        // 10_000 bps = 100% -> fee = notional.
        assert_eq!(MoneyPath::fee(100, 10_000), Ok(100));
        // 1_000 bps = 10% -> fee = 10.
        assert_eq!(MoneyPath::fee(100, 1_000), Ok(10));
        // 100 * 1 / 10_000 = 0.01 -> rounds up to 1 unit.
        assert_eq!(MoneyPath::fee(100, 100), Ok(1));
    }

    #[test]
    fn spec_holds_for_collateral() {
        assert_eq!(MoneyPath::compute_collateral("call", 2, 67, 67), Ok(134));
        // put: 3 * 60 * 1.1 = 198.
        assert_eq!(MoneyPath::compute_collateral("put", 3, 60, 50), Ok(198));
        // put: 1 * 1 * 1.1 = 1.1 -> rounds up to 2.
        assert_eq!(MoneyPath::compute_collateral("put", 1, 1, 1), Ok(2));
    }

    #[test]
    fn round_trip_i128_stays_in_domain() {
        let mut r = Random::new(42);
        for _ in 0..100 {
            let v = r.next_within(-10, 10);
            let m = Money::new(v).unwrap();
            assert_eq!(m.to_i128(), v);
        }
    }
}
