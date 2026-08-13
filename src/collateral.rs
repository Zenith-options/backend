//! Collateral requirements for writing (selling) options, ported bit-for-bit
//! from the frontend's `lib/collateral.ts`: covered calls are 100% covered
//! by the underlying's current value, cash-secured puts are
//! over-collateralized by 110% of the strike (protects against a further
//! drop before the writer can react). Only applies to the short/write
//! side — buying an option never requires collateral, just the premium.

pub fn collateral_required(option_type: &str, contracts: f64, strike: f64, spot: f64) -> f64 {
    if option_type == "call" {
        contracts * spot
    } else {
        contracts * strike * 1.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covered_call_is_100_percent_of_spot() {
        assert_eq!(collateral_required("call", 2.0, 70000.0, 67420.50), 2.0 * 67420.50);
    }

    #[test]
    fn cash_secured_put_is_110_percent_of_strike() {
        assert_eq!(collateral_required("put", 3.0, 60000.0, 67420.50), 3.0 * 60000.0 * 1.1);
    }
}
