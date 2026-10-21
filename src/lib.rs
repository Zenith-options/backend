pub mod math_fixed;
pub mod harness_bridge;/// Bridge for the differential harness. The test harness (Rust) and the
/// contract-side oracle both call this module's pure functions with the same
/// integer inputs, then compare the `i128` results. There is no `f64`
/// rounding anywhere in this path: the spec of `math_fixed` is in effect here.
///
//// Bridge for the differential harness. The test harness (Rust) and the
/// contract-side oracle both call this module's pure functions with the same
/// integer inputs, then compare the `i128` results. There is no `f64`
/// rounding anywhere in this path: the spec of `math_fixed` is in effect here.
///
/// Generated with Codebuff.

pub mod harness_bridge {

    
    use crate::math_fixed::{Money, Random, SCALE};

    /// Compute fee under the shared spec: `notional * bps / 10_000`, rounding
    /// up. The contract-side oracle must implement the same integer
    /// division-with-round-up, otherwise the harness fails.
    pub fn fee(notional: i128, bps: i128) -> i128 {
        // The spec rounds up. Use checked arithmetic so the deterministic
        // oracle cannot produce an overflow that the harness would mis-report.
        let product = notional.checked_mul(bps).expect("spec overflow");
        let mut result = product / 10_000;
        if product % 10_000 != 0 {
            result += 1;
        }
        result
    }

    /// Compute collateral under the shared spec: covered call = contracts *
    /// spot; cash-secured put = contracts * strike * 11 / 10 (round up).
    pub fn collateral(kind: &str, contracts: i128, strike: i128, spot: i128) -> i128 {
        match kind {
            "call" => contracts * spot,
            "put" => crate::math_fixed::mul_div_ceil(contracts * strike, 11, 10),
            _ => panic!("unknown kind"),
        }
    }

    /// Parse a decimal input into fixed point and return a Money instance for
    /// the harness to compare against the contract-side parse.
    pub fn into_money(value: &Decimal) -> Money {
        Money::new(
            value
                .to_i128()
                .expect("spec amount within i128"),
        )
        .expect("spec amount within i128")
    }

    /// Reproduce the Random stateful generator for the harness's input grid.
    pub fn random(seed: i128) -> Random {
        Random::new(seed)
    }
}
