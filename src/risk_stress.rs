use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{db_error, AppError};
use crate::models::Position;
use crate::{black_scholes, AppState, BSInputs};

pub const MAX_STRESS_SCENARIOS: usize = 2_500;
const MIN_VOL: f64 = 0.01;
const RISK_FREE_RATE: f64 = 0.05;

#[derive(Debug, Clone, Deserialize)]
pub struct StressScenario {
    pub name: String,
    /// Relative spot shock for each underlying (for example `-0.2` for -20%).
    pub spot_shocks_pct: BTreeMap<String, f64>,
    /// Absolute volatility shocks for each underlying (for example `0.15`).
    pub vol_shocks_abs: BTreeMap<String, f64>,
    pub days_forward: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StressRequest {
    pub scenarios: Vec<StressScenario>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioProjection {
    pub name: String,
    pub pnl: f64,
    pub projected_equity: f64,
    pub projected_margin: f64,
    pub margin_shortfall: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortfolioStressResult {
    pub capital: f64,
    pub worst_pnl: f64,
    pub worst_margin_shortfall: f64,
    pub scenarios: Vec<ScenarioProjection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StressResponse {
    pub portfolio: PortfolioStressResult,
    /// Aggregated paper accounts and positions. This is a reserve-sizing
    /// simulation, not a claim about a deployed on-chain vault balance.
    pub pooled_vault: PortfolioStressResult,
}

#[derive(Debug, Clone)]
struct StressLeg {
    underlying: String,
    spot: f64,
    strike: f64,
    time_to_expiry: f64,
    vol: f64,
    quantity: f64,
    is_call: bool,
}

pub async fn stress_portfolios(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    Json(request): Json<StressRequest>,
) -> Result<Json<StressResponse>, AppError> {
    validate_request(&request)?;
    validate_underlying_symbols(&state, &request)?;

    let user_positions = load_open_positions(&state, &wallet).await?;
    let all_positions: Vec<Position> = sqlx::query_as(
        "SELECT * FROM positions WHERE status = 'open' ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|error| db_error("load pooled positions for stress test", error))?;

    let user_capital: f64 =
        sqlx::query_scalar("SELECT balance FROM accounts WHERE wallet_address = ?")
            .bind(&wallet)
            .fetch_one(&state.db)
            .await
            .map_err(|error| db_error("load account capital for stress test", error))?;
    let pooled_capital: f64 = sqlx::query_scalar("SELECT COALESCE(SUM(balance), 0.0) FROM accounts")
        .fetch_one(&state.db)
        .await
        .map_err(|error| db_error("load pooled capital for stress test", error))?;

    let portfolio_legs = make_legs(&state, &user_positions)?;
    let pooled_legs = make_legs(&state, &all_positions)?;
    let portfolio = run_stress(&portfolio_legs, user_capital, &request.scenarios);
    let pooled_vault = run_stress(&pooled_legs, pooled_capital, &request.scenarios);

    Ok(Json(StressResponse {
        portfolio,
        pooled_vault,
    }))
}

async fn load_open_positions(state: &AppState, wallet: &str) -> Result<Vec<Position>, AppError> {
    sqlx::query_as(
        "SELECT * FROM positions WHERE wallet_address = ? AND status = 'open' ORDER BY id",
    )
    .bind(wallet)
    .fetch_all(&state.db)
    .await
    .map_err(|error| db_error("load portfolio positions for stress test", error))
}

fn validate_request(request: &StressRequest) -> Result<(), AppError> {
    if request.scenarios.is_empty() || request.scenarios.len() > MAX_STRESS_SCENARIOS {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "scenarios must contain between 1 and {MAX_STRESS_SCENARIOS} entries"
            ),
        ));
    }

    for scenario in &request.scenarios {
        if scenario.name.trim().is_empty()
            || !scenario.days_forward.is_finite()
            || !(0.0..=36_500.0).contains(&scenario.days_forward)
            || scenario
                .spot_shocks_pct
                .values()
                .any(|shock| !shock.is_finite() || *shock <= -1.0 || *shock > 5.0)
            || scenario
                .vol_shocks_abs
                .values()
                .any(|shock| !shock.is_finite() || shock.abs() > 5.0)
        {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "scenario names, forward days, and shock values must be valid and finite",
            ));
        }
    }
    Ok(())
}

fn validate_underlying_symbols(
    state: &AppState,
    request: &StressRequest,
) -> Result<(), AppError> {
    let known: std::collections::HashSet<String> =
        state.spot_prices.lock().unwrap().keys().cloned().collect();
    for scenario in &request.scenarios {
        if scenario
            .spot_shocks_pct
            .keys()
            .chain(scenario.vol_shocks_abs.keys())
            .any(|underlying| !known.contains(underlying))
        {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "scenario shocks contain an unknown underlying",
            ));
        }
    }
    Ok(())
}

fn make_legs(state: &AppState, positions: &[Position]) -> Result<Vec<StressLeg>, AppError> {
    let now = current_unix_seconds()?;
    let prices = state.spot_prices.lock().unwrap();
    let vols = state.vol_surface.lock().unwrap();
    positions
        .iter()
        .map(|position| {
            let spot = *prices.get(&position.underlying).ok_or_else(|| {
                AppError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("no current spot configured for {}", position.underlying),
                )
            })?;
            let vol = *vols.get(&position.underlying).ok_or_else(|| {
                AppError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("no current volatility configured for {}", position.underlying),
                )
            })?;
            let direction = if position.position_type == "long" {
                1.0
            } else {
                -1.0
            };
            Ok(StressLeg {
                underlying: position.underlying.clone(),
                spot,
                strike: position.strike.to_f64(),
                time_to_expiry: crate::positions::time_to_expiry_years(
                    &position.expires_at,
                    now,
                ),
                vol,
                quantity: position.contracts.to_f64() * direction,
                is_call: position.option_type == "call",
            })
        })
        .collect()
}

fn current_unix_seconds() -> Result<i64, AppError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "system clock is before the Unix epoch",
            )
        })?
        .as_secs()
        .try_into()
        .map_err(|_| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "system timestamp exceeds supported range",
            )
        })
}

fn run_stress(
    legs: &[StressLeg],
    capital: f64,
    scenarios: &[StressScenario],
) -> PortfolioStressResult {
    let mut projections = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let mut pnl = 0.0;
        let mut projected_margin = 0.0;
        for leg in legs {
            let spot = leg.spot;
            let shock = scenario
                .spot_shocks_pct
                .get(&leg.underlying)
                .copied()
                .unwrap_or(0.0);
            let vol_shock = scenario
                .vol_shocks_abs
                .get(&leg.underlying)
                .copied()
                .unwrap_or(0.0);
            let current_price = price(spot, leg.strike, leg.time_to_expiry, leg.vol, leg.is_call);
            let forward_time = scenario.days_forward / 365.0;
            let stressed_price = price(
                spot * (1.0 + shock),
                leg.strike,
                (leg.time_to_expiry - forward_time).max(0.0),
                (leg.vol + vol_shock).max(MIN_VOL),
                leg.is_call,
            );
            pnl += (stressed_price - current_price) * leg.quantity;
            if leg.quantity < 0.0 {
                projected_margin += stressed_price * leg.quantity.abs();
            }
        }

        let projected_equity = capital + pnl;
        projections.push(ScenarioProjection {
            name: scenario.name.clone(),
            pnl,
            projected_equity,
            projected_margin,
            margin_shortfall: (projected_margin - projected_equity).max(0.0),
        });
    }

    let worst_pnl = projections
        .iter()
        .map(|projection| projection.pnl)
        .fold(f64::INFINITY, f64::min);
    let worst_margin_shortfall = projections
        .iter()
        .map(|projection| projection.margin_shortfall)
        .fold(0.0, f64::max);

    PortfolioStressResult {
        capital,
        worst_pnl: if worst_pnl.is_finite() { worst_pnl } else { 0.0 },
        worst_margin_shortfall,
        scenarios: projections,
    }
}

fn price(spot: f64, strike: f64, time: f64, vol: f64, is_call: bool) -> f64 {
    black_scholes(&BSInputs {
        spot,
        strike,
        vol,
        t: time,
        r: RISK_FREE_RATE,
        is_call,
    })
    .premium
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(quantity: f64) -> StressLeg {
        StressLeg {
            underlying: "BTC".into(),
            spot: 100.0,
            strike: 100.0,
            time_to_expiry: 1.0,
            vol: 0.2,
            quantity,
            is_call: true,
        }
    }

    #[test]
    fn correlated_shocks_apply_per_underlying_and_report_shortfall() {
        let scenario = StressScenario {
            name: "btc-up-vol-up".into(),
            spot_shocks_pct: BTreeMap::from([("BTC".into(), 1.0)]),
            vol_shocks_abs: BTreeMap::from([("BTC".into(), 0.3)]),
            days_forward: 0.0,
        };
        let result = run_stress(&[leg(-1.0)], 0.0, &[scenario]);
        let projection = &result.scenarios[0];
        assert!(projection.projected_margin > 0.0);
        assert!(projection.margin_shortfall > 0.0);
    }

    #[test]
    fn request_rejects_more_than_the_scenario_cap() {
        let request = StressRequest {
            scenarios: (0..=MAX_STRESS_SCENARIOS)
                .map(|index| StressScenario {
                    name: index.to_string(),
                    spot_shocks_pct: BTreeMap::new(),
                    vol_shocks_abs: BTreeMap::new(),
                    days_forward: 0.0,
                })
                .collect(),
        };
        assert!(validate_request(&request).is_err());
    }
}
