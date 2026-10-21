//! Collateral requirements for writing (selling) options.
//!
//! The live app still uses `f64` for pricing and balance calculations, but the
//! legacy write-side collateral rules were intentionally rewritten as a fixed-
//! point [`Money`] type so the protocol can round fees/collateral away from the
//! user without losing precision. The public helpers below are therefore pure,
//! finite, and testable.

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal::RoundingStrategy;

/// Fixed-point monetary amount backed by `rust_decimal::Decimal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Money(Decimal);

impl Money {
    /// Explicit conversion boundary from the `f64` pricing math.
    pub fn from_price(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        Decimal::from_f64_retain(value).map(Money)
    }

    pub fn round_against_user(self, scale: u32) -> Self {
        Money(self.0.round_dp_with_strategy(scale, RoundingStrategy::AwayFromZero))
    }

    pub fn round_for_display(self, scale: u32) -> Self {
        Money(self.0.round_dp_with_strategy(scale, RoundingStrategy::MidpointNearestEven))
    }

    pub fn checked_mul(self, rhs: Self) -> Option<Self> {
        self.0.checked_mul(rhs.0).map(Money)
    }

    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        self.0.checked_add(rhs.0).map(Money)
    }

    pub fn value(self) -> Decimal {
        self.0
    }
}

impl std::fmt::Display for Money {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.round_for_display(7).0)
    }
}

/// A single option leg held by a wallet, as seen by the margin engine.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub option_type: String,
    pub contracts: f64,
    pub strike: f64,
    pub spot: f64,
    pub vol: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorstScenario {
    pub spot_shock: f64,
    pub vol_shock: f64,
    pub loss: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarginRequirement {
    pub initial: f64,
    pub maintenance: f64,
    pub worst_scenario: WorstScenario,
    pub contributions: Vec<f64>,
}

const MAINTENANCE_FRACTION: f64 = 0.75;
const SPOT_SHOCKS: [f64; 7] = [-0.30, -0.15, -0.05, 0.0, 0.05, 0.15, 1.0];
const VOL_SHOCKS: [f64; 3] = [-0.20, 0.0, 0.20];

pub trait MarginModel {
    fn requirement(&self, positions: &[Position]) -> MarginRequirement;
    fn exceeds_equity(&self, positions: &[Position], equity: f64) -> bool {
        self.requirement(positions).initial > equity
    }
}

pub struct StrategyBasedMargin;

impl MarginModel for StrategyBasedMargin {
    fn requirement(&self, positions: &[Position]) -> MarginRequirement {
        let mut contributions = Vec::with_capacity(positions.len());
        let mut initial = 0.0;

        for p in positions {
            let c = if p.contracts < 0.0 {
                collateral_required(&p.option_type, -p.contracts, p.strike, p.spot)
                    .map(|m| m.value().to_f64().unwrap_or(0.0))
                    .unwrap_or(0.0)
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

fn leg_loss(p: &Position, spot_shock: f64, vol_shock: f64) -> f64 {
    let shocked_spot = p.spot * (1.0 + spot_shock);
    let shocked_vol = (p.vol + vol_shock).max(0.0);
    let base = option_value(&p.option_type, p.spot, p.strike, p.vol);
    let shocked = option_value(&p.option_type, shocked_spot, p.strike, shocked_vol);
    -p.contracts * (shocked - base)
}

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

/// Collateral required to write an option, as fixed-point [`Money`].
pub fn collateral_required(
    option_type: &str,
    contracts: f64,
    strike: f64,
    spot: f64,
) -> Option<Money> {
    if !contracts.is_finite() || !strike.is_finite() || !spot.is_finite() {
        return None;
    }

    let contracts = Money::from_price(contracts)?;
    let amount = match option_type {
        "call" => {
            let spot = Money::from_price(spot)?;
            contracts.checked_mul(spot)?
        }
        "put" => {
            let strike = Money::from_price(strike)?;
            let buffer = Money::from_price(1.1)?;
            contracts.checked_mul(strike)?.checked_mul(buffer)?
        }
        _ => return None,
    };

    Some(amount.round_against_user(7))
}

pub struct RemmarginConfig {
    pub drift_threshold: f64,
    pub hysteresis: f64,
}

impl Default for RemmarginConfig {
    fn default() -> Self {
        Self {
            drift_threshold: 0.05,
            hysteresis: 0.02,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum RemmarginAction {
    None,
    TopUp { amount: f64 },
    Release { amount: f64 },
}

pub fn remargin_action(
    option_type: &str,
    contracts: f64,
    strike: f64,
    spot: f64,
    locked: f64,
    config: &RemmarginConfig,
) -> RemmarginAction {
    let required = collateral_required(option_type, contracts, strike, spot)
        .map(|m| m.value().to_f64().unwrap_or(locked))
        .unwrap_or(locked);
    let drift = required - locked;

    if locked <= 0.0 {
        return RemmarginAction::None;
    }

    let threshold = locked * config.drift_threshold;
    if drift > threshold {
        RemmarginAction::TopUp { amount: drift }
    } else if -drift > threshold + locked * config.hysteresis {
        RemmarginAction::Release { amount: -drift }
    } else {
        RemmarginAction::None
    }
}

pub fn remmargin_action(
    option_type: &str,
    contracts: f64,
    strike: f64,
    spot: f64,
    locked: f64,
    config: &RemmarginConfig,
) -> RemmarginAction {
    remargin_action(option_type, contracts, strike, spot, locked, config)
}

pub async fn check_collateral_sum(
    db: &sqlx::SqlitePool,
) -> Result<Vec<(String, f64, f64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT a.wallet_address, a.collateral_locked, COALESCE(SUM(p.collateral), 0.0)
           FROM accounts a
           LEFT JOIN positions p ON p.wallet_address = a.wallet_address AND p.status = 'open'
          GROUP BY a.wallet_address, a.collateral_locked
         HAVING a.collateral_locked != COALESCE(SUM(p.collateral), 0.0)",
    )
    .fetch_all(db)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covered_call_is_100_percent_of_spot() {
        let got = collateral_required("call", 2.0, 70000.0, 67420.50).unwrap();
        let want = Money::from_price(2.0 * 67420.50).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn cash_secured_put_is_110_percent_of_strike() {
        let got = collateral_required("put", 3.0, 60000.0, 67420.50).unwrap();
        let want = Money::from_price(3.0 * 60000.0 * 1.1).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn rejects_nan_and_inf() {
        assert!(collateral_required("call", f64::NAN, 70000.0, 67420.50).is_none());
        assert!(collateral_required("call", 2.0, 70000.0, f64::INFINITY).is_none());
        assert!(Money::from_price(f64::NEG_INFINITY).is_none());
    }

    #[test]
    fn collateral_rounds_against_the_user() {
        let got = collateral_required("put", 1.0, 0.00000001, 1.0).unwrap();
        assert_eq!(got.value(), rust_decimal::dec!(0.0000001));
    }

    #[test]
    fn spot_rise_triggers_top_up() {
        let cfg = RemmarginConfig::default();
        let action = remmargin_action("call", 1.0, 0.0, 200.0, 100.0, &cfg);
        assert_eq!(action, RemmarginAction::TopUp { amount: 100.0 });
    }

    #[test]
    fn spot_fall_triggers_release() {
        let cfg = RemmarginConfig::default();
        let action = remmargin_action("call", 1.0, 0.0, 100.0, 200.0, &cfg);
        assert_eq!(action, RemmarginAction::Release { amount: 100.0 });
    }

    #[test]
    fn hysteresis_prevents_flapping() {
        let cfg = RemmarginConfig::default();
        assert_eq!(
            remmargin_action("call", 1.0, 0.0, 104.0, 100.0, &cfg),
            RemmarginAction::None
        );
        assert_eq!(
            remmargin_action("call", 1.0, 0.0, 96.0, 100.0, &cfg),
            RemmarginAction::None
        );
    }

    #[tokio::test]
    async fn check_collateral_sum_flags_a_drifted_wallet() {
        let db_path =
            std::env::temp_dir().join(format!("zenith-collateral-test-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", db_path.display())).await;

        sqlx::query("INSERT INTO accounts (wallet_address, collateral_locked) VALUES ('GTEST', 100)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO positions
                (id, wallet_address, underlying, strike, expiry_days, option_type,
                 position_type, contracts, entry_premium, entry_spot, collateral, status)
             VALUES ('p1', 'GTEST', 'BTC', 70000, 30, 'call', 'short', 1, 100, 67000, 100, 'open')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let drift = check_collateral_sum(&pool).await.unwrap();
        assert!(drift.is_empty(), "no drift expected: {drift:?}");

        sqlx::query("UPDATE accounts SET collateral_locked = 50 WHERE wallet_address = 'GTEST'")
            .execute(&pool)
            .await
            .unwrap();

        let drift = check_collateral_sum(&pool).await.unwrap();
        assert_eq!(drift.len(), 1, "the drifted wallet must be reported");
        let (wallet, locked, sum) = &drift[0];
        assert_eq!(wallet, "GTEST");
        assert_eq!(*locked, 50.0);
        assert_eq!(*sum, 100.0);

        pool.close().await;
        let _ = std::fs::remove_file(&db_path);
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
