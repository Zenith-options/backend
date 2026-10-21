//! Deterministic binary the differential test harness links against.
//!
//! The harness (Rust) and the contract-side oracle both call these pure
//! functions with the same integer inputs and compare `i128` results. There is
//! no `f64` rounding anywhere in this path: the spec of `math_fixed` is in
//! effect here.

/// Compute fee under the shared spec: `notional * bps / 10_000`, rounding up.
pub fn fee(notional: i128, bps: i128) -> i128 {
    let product = notional.checked_mul(bps).expect("spec overflow");
    let mut result = product / 10_000;
    if product % 10_000 != 0 {
        result += 1;
    }
    result
}

/// Compute collateral under the shared spec: covered call = contracts * spot;
/// cash-secured put = contracts * strike * 11 / 10 (round up).
pub fn collateral(kind: &str, contracts: i128, strike: i128, spot: i128) -> i128 {
    match kind {
        "call" => contracts * spot,
        "put" => crate::math_fixed::mul_div_ceil(contracts * strike, 11, 10),
        _ => panic!("unknown kind"),
    }
}
