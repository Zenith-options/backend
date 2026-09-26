//! Collateral requirements for writing (selling) options, ported bit-for-bit
//! from the frontend's `lib/collateral.ts`: covered calls are 100% covered
//! by the underlying's current value, cash-secured puts are
//! over-collateralized by 110% of the strike (protects against a further
//! drop before the writer can react). Only applies to the short/write
//! side — buying an option never requires collateral, just the premium.
//!
//! Issue #24 introduces a portfolio margin engine on top of these legacy
//! per-leg rules. The legacy rules are preserved verbatim as
//! [`StrategyBasedMargin`] (the fallback), while [`RiskArrayMargin`]
//! computes a wallet's requirement as the worst-case loss across a stress
//! grid of spot shocks × vol shocks (SPAN-style), so hedged, defined-risk
//! strategies such as spreads, iron condors and butterflies only lock their
//! true maximum loss. Both implement the [`MarginModel`] trait so callers
//! (open/close/roll/strategy execution) can select a model per environment
//! via a feature flag.

/// A single option leg held by a wallet, as seen by the margin engine.
///
/// `contracts` is signed: positive for long, negative for short. `strike`
/// and `spot` are in the same quote currency. `vol` is the implied
/// volatility used to reprice the leg under a vol shock.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub option_type: String,
    pub contracts: f64,
    pub strike: f64,
    pub spot: f64,
    pub vol: f64,
}

/// The worst scenario found on the stress grid, reported by
/// `GET /api/v1/account/margin` alongside the requirement.
#[derive(Debug, Clone, PartialEq)]
pub struct WorstScenario {
    pub spot_shock: f64,
    pub vol_shock: f64,
    pub loss: f64,
}

/// A wallet's margin requirement, broken down for the margin endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct MarginRequirement {
    /// Initial requirement: the worst-case loss across the stress grid.
    pub initial: f64,
    /// Maintenance requirement: a fraction of the initial requirement.
    pub maintenance: f64,
    /// The scenario that produced the initial requirement.
    pub worst_scenario: WorstScenario,
    /// Per-position contribution to the initial requirement.
    pub contributions: Vec<f64>,
}

/// Fraction of the initial requirement that must be maintained.
const MAINTENANCE_FRACTION: f64 = 0.75;

/// Spot shocks applied to every leg, as fractions of the current spot.
/// The top shock is large enough (+100%) to bound the otherwise unbounded
/// upside of a naked short call.
const SPOT_SHOCKS: [f64; 7] = [-0.30, -0.15, -0.05, 0.0, 0.05, 0.15, 1.0];

/// Volatility shocks applied to every leg, as absolute vol points.
const VOL_SHOCKS: [f64; 3] = [-0.20, 0.0, 0.20];

/// A margin model computes a wallet's requirement from its post-trade
/// position set. Implementations must be pure so the what-if check and the
/// commit see the same numbers inside one transaction.
pub trait MarginModel {
    /// Compute the requirement for the given post-trade positions.
    fn requirement(&self, positions: &[Position]) -> MarginRequirement;

    /// Whether the post-trade initial margin would exceed `equity`.
    /// Used to reject a trade with 422 before commit.
    fn exceeds_equity(&self, positions: &[Position], equity: f64) -> bool {
        self.requirement(positions).initial > equity
    }
}

/// Legacy per-leg rules, kept as the fallback model. Short calls lock 100%
/// of spot, short puts lock 110% of strike; long legs require nothing.
pub struct StrategyBasedMargin;

impl MarginModel for StrategyBasedMargin {
    fn requirement(&self, positions: &[Position]) -> MarginRequirement {
        let mut contributions = Vec::with_capacity(positions.len());
        let mut initial = 0.0;
        for p in positions {
            let c = if p.contracts < 0.0 {
                collateral_required(&p.option_type, -p.contracts, p.strike, p.spot)
            } else {
                0.0
            };
            initial += c;
            contributions.push(c);
        }
        MarginRequirement {
            initial,
            maintenance: initial * MAINTENANCE_FRACTION,
            worst_scenario: WorstScenario {
                spot_shock: 0.0,
                vol_shock: 0.0,
                loss: initial,
            },
            contributions,
        }
    }
}

/// SPAN-style stress grid: reprices every leg under each spot × vol shock
/// and takes the worst-case loss as the requirement. Hedged, defined-risk
/// strategies therefore only lock their true maximum loss.
pub struct RiskArrayMargin;

impl MarginModel for RiskArrayMargin {
    fn requirement(&self, positions: &[Position]) -> MarginRequirement {
        let mut worst = WorstScenario {
            spot_shock: 0.0,
            vol_shock: 0.0,
            loss: 0.0,
        };
        for &spot_shock in &SPOT_SHOCKS {
            for &vol_shock in &VOL_SHOCKS {
                let loss: f64 = positions
                    .iter()
                    .map(|p| leg_loss(p, spot_shock, vol_shock))
                    .sum();
                if loss > worst.loss {
                    worst = WorstScenario {
                        spot_shock,
                        vol_shock,
                        loss,
                    };
                }
            }
        }
        let initial = worst.loss.max(0.0);
        let contributions = positions
            .iter()
            .map(|p| leg_loss(p, worst.spot_shock, worst.vol_shock).max(0.0))
            .collect();
        MarginRequirement {
            initial,
            maintenance: initial * MAINTENANCE_FRACTION,
            worst_scenario: worst,
            contributions,
        }
    }
}

/// Loss of a single leg under a spot and vol shock. Long legs lose when the
/// option loses value; short legs lose when it gains value.
fn leg_loss(p: &Position, spot_shock: f64, vol_shock: f64) -> f64 {
    let shocked_spot = p.spot * (1.0 + spot_shock);
    let shocked_vol = (p.vol + vol_shock).max(0.0);
    let base = option_value(&p.option_type, p.spot, p.strike, p.vol);
    let shocked = option_value(&p.option_type, shocked_spot, p.strike, shocked_vol);
    // A short position (negative contracts) loses when the option value rises.
    -p.contracts * (shocked - base)
}

/// Intrinsic-plus-time option value used for repricing. Kept deliberately
/// simple and deterministic so the grid is reproducible; the time value
/// scales with vol and decays as the option moves out of the money.
fn option_value(option_type: &str, spot: f64, strike: f64, vol: f64) -> f64 {
    let intrinsic = if option_type == "call" {
        (spot - strike).max(0.0)
    } else {
        (strike - spot).max(0.0)
    };
    let moneyness = (spot - strike).abs() / strike.max(f64::EPSILON);
    let time_value = spot * vol * (1.0 - moneyness).max(0.0);
    intrinsic + time_value
}

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
    fn strategy_based_matches_legacy_rules() {
        let positions = vec![
            Position {
                option_type: "call".into(),
                contracts: -2.0,
                strike: 70000.0,
                spot: 67420.50,
                vol: 0.6,
            },
            Position {
                option_type: "put".into(),
                contracts: -3.0,
                strike: 60000.0,
                spot: 67420.50,
                vol: 0.6,
            },
        ];
        let req = StrategyBasedMargin.requirement(&positions);
        assert_eq!(req.initial, 2.0 * 67420.50 + 3.0 * 60000.0 * 1.1);
        assert_eq!(req.maintenance, req.initial * MAINTENANCE_FRACTION);
    }

    #[test]
    fn long_legs_require_no_collateral() {
        let positions = vec![Position {
            option_type: "call".into(),
            contracts: 1.0,
            strike: 70000.0,
            spot: 67420.50,
            vol: 0.6,
        }];
        assert_eq!(StrategyBasedMargin.requirement(&positions).initial, 0.0);
    }

    #[test]
    fn vertical_spread_locks_width_minus_credit() {
        // 1-wide bull put spread: short 60000 put, long 59000 put.
        let positions = vec![
            Position {
                option_type: "put".into(),
                contracts: -1.0,
                strike: 60000.0,
                spot: 60000.0,
                vol: 0.0,
            },
            Position {
                option_type: "put".into(),
                contracts: 1.0,
                strike: 59000.0,
                spot: 60000.0,
                vol: 0.0,
            },
        ];
        let req = RiskArrayMargin.requirement(&positions);
        // Worst case is the full width (1000) at the deepest down shock.
        assert!(req.initial <= 1000.0 + 1e-9);
        assert!(req.initial > 0.0);
    }

    #[test]
    fn naked_short_call_is_bounded_by_top_shock() {
        let positions = vec![Position {
            option_type: "call".into(),
            contracts: -1.0,
            strike: 70000.0,
            spot: 67420.50,
            vol: 0.0,
        }];
        let req = RiskArrayMargin.requirement(&positions);
        // +100% spot shock bounds the unbounded upside.
        assert!(req.initial > 0.0);
        assert_eq!(req.worst_scenario.spot_shock, 1.0);
    }

    #[test]
    fn exceeds_equity_rejects_under_margined_trade() {
        let positions = vec![Position {
            option_type: "call".into(),
            contracts: -1.0,
            strike: 70000.0,
            spot: 67420.50,
            vol: 0.0,
        }];
        assert!(RiskArrayMargin.exceeds_equity(&positions, 1.0));
        assert!(!RiskArrayMargin.exceeds_equity(&positions, 1e12));
    }
}
