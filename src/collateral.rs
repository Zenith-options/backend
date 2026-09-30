//! Collateral requirements for writing (selling) options, ported bit-for-bit
//! from the frontend's `lib/collateral.ts`: covered calls are 100% covered
//! by the underlying's current value, cash-secured puts are
//! over-collateralized by 110% of the strike (protects against a further
//! drop before the writer can react). Only applies to the short/write
//! side — buying an option never requires collateral, just the premium.
//!
//! Dynamic re-margining (issue #41): a short position's collateral is fixed
//! at open, so a spot move silently under- or over-collateralises it. The
//! helpers below recompute the required collateral and decide whether a
//! top-up (draw from free balance) or a release (return excess to free
//! balance) is warranted, using hysteresis bands so the account does not
//! flap around the trigger threshold.

pub fn collateral_required(option_type: &str, contracts: f64, strike: f64, spot: f64) -> f64 {
    if option_type == "call" {
        contracts * spot
    } else {
        contracts * strike * 1.1
    }
}

/// Configuration for the re-margining loop. `drift_threshold` is the
/// configurable X% (as a fraction, e.g. 0.05 for 5%) that required
/// collateral must drift from the locked amount before a re-margin is
/// triggered. `hysteresis` is the fraction of the locked amount that must
/// be recovered before a release is allowed, preventing flapping.
pub struct RemarginConfig {
    pub drift_threshold: f64,
    pub hysteresis: f64,
}

impl Default for RemarginConfig {
    fn default() -> Self {
        Self {
            drift_threshold: 0.05,
            hysteresis: 0.02,
        }
    }
}

/// The action the re-margining loop should take for a single short position.
#[derive(Debug, PartialEq)]
pub enum RemarginAction {
    /// No drift beyond the threshold; leave collateral untouched.
    None,
    /// Required collateral rose; draw `amount` from free balance.
    TopUp { amount: f64 },
    /// Required collateral fell; return `amount` to free balance.
    Release { amount: f64 },
}

/// Decide whether an open short position needs re-margining given the
/// current spot price. `locked` is the collateral currently held for the
/// position and `free_balance` is the wallet's available balance.
///
/// Returns the action to apply. A top-up larger than the free balance is
/// still returned as `TopUp`; the caller is responsible for entering
/// `margin_call` when the free balance is insufficient (hooked into the
/// liquidation engine).
pub fn remargin_action(
    option_type: &str,
    contracts: f64,
    strike: f64,
    spot: f64,
    locked: f64,
    config: &RemarginConfig,
) -> RemarginAction {
    let required = collateral_required(option_type, contracts, strike, spot);
    let drift = required - locked;

    if locked <= 0.0 {
        return RemarginAction::None;
    }

    let threshold = locked * config.drift_threshold;

    if drift > threshold {
        RemarginAction::TopUp { amount: drift }
    } else if -drift > threshold + locked * config.hysteresis {
        RemarginAction::Release { amount: -drift }
    } else {
        RemarginAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covered_call_is_100_percent_of_spot() {
        assert_eq!(
            collateral_required("call", 2.0, 70000.0, 67420.50),
            2.0 * 67420.50
        );
    }

    #[test]
    fn cash_secured_put_is_110_percent_of_strike() {
        assert_eq!(
            collateral_required("put", 3.0, 60000.0, 67420.50),
            3.0 * 60000.0 * 1.1
        );
    }

    #[test]
    fn spot_rise_triggers_top_up() {
        let cfg = RemarginConfig::default();
        // 1 call locked at spot 100 -> 100. Spot doubles to 200.
        let action = remargin_action("call", 1.0, 0.0, 200.0, 100.0, &cfg);
        assert_eq!(action, RemarginAction::TopUp { amount: 100.0 });
    }

    #[test]
    fn spot_fall_triggers_release() {
        let cfg = RemarginConfig::default();
        // 1 call locked at spot 200 -> 200. Spot falls to 100.
        let action = remargin_action("call", 1.0, 0.0, 100.0, 200.0, &cfg);
        assert_eq!(action, RemarginAction::Release { amount: 100.0 });
    }

    #[test]
    fn hysteresis_prevents_flapping() {
        let cfg = RemarginConfig::default();
        // Required 104 vs locked 100: 4% drift, below the 5% threshold.
        assert_eq!(
            remargin_action("call", 1.0, 0.0, 104.0, 100.0, &cfg),
            RemarginAction::None
        );
        // Required 96 vs locked 100: 4% drift, below threshold + hysteresis.
        assert_eq!(
            remargin_action("call", 1.0, 0.0, 96.0, 100.0, &cfg),
            RemarginAction::None
        );
    }
}
