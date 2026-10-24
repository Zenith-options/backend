use axum::{extract::State, http::HeaderMap, http::StatusCode, response::Json};
use serde::{Deserialize, Serialize};

use crate::{black_scholes, smile_vol, AppState, BSInputs};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementAction {
    Open,
    Close,
    Exercise,
    Deposit,
    Withdraw,
    Substitution,
    AssignedSettlement,
}

#[derive(Debug, Deserialize)]
pub struct SettlementPreviewRequest {
    pub action: SettlementAction,
    pub position_id: Option<String>,
    pub underlying: Option<String>,
    pub strike: Option<f64>,
    pub expiry_days: Option<f64>,
    pub option_type: Option<String>,
    pub position_type: Option<String>,
    pub contracts: Option<f64>,
    pub amount: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct SettlementPreview {
    pub action: SettlementAction,
    pub spot: Option<f64>,
    pub option_premium: Option<f64>,
    pub cash_delta: f64,
    pub collateral_delta: f64,
    pub current_margin: Option<f64>,
    pub post_action_margin: Option<f64>,
    pub estimated_fee: Option<f64>,
    pub estimated_gas_units: Option<u64>,
    pub violations: Vec<String>,
    pub estimates_unavailable: Vec<String>,
}

#[derive(Debug, Clone)]
struct PositionRow {
    id: String,
    underlying: String,
    strike: f64,
    expiry_days: f64,
    option_type: String,
    position_type: String,
    contracts: f64,
    entry_premium: f64,
    collateral: f64,
}

pub async fn preview_settlement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SettlementPreviewRequest>,
) -> Result<Json<SettlementPreview>, StatusCode> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let wallet = sqlx::query_scalar::<_, String>(
        "SELECT wallet_address FROM sessions
         WHERE token = ?1 AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(token)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "settlement preview session lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::UNAUTHORIZED)?;

    let (balance, collateral_locked) = sqlx::query_as::<_, (f64, f64)>(
        "SELECT balance, collateral_locked FROM accounts WHERE wallet_address = ?1",
    )
    .bind(&wallet)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "settlement preview account lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::UNAUTHORIZED)?;

    let rows = sqlx::query_as::<_, (String, String, f64, f64, String, String, f64, f64, f64)>(
        "SELECT id, underlying, strike, expiry_days, option_type, position_type,
                contracts, entry_premium, collateral
         FROM positions WHERE wallet_address = ?1 AND status = 'open'",
    )
    .bind(&wallet)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "settlement preview positions lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let mut positions: Vec<PositionRow> = rows
        .into_iter()
        .map(|row| PositionRow {
            id: row.0,
            underlying: row.1,
            strike: row.2,
            expiry_days: row.3,
            option_type: row.4,
            position_type: row.5,
            contracts: row.6,
            entry_premium: row.7,
            collateral: row.8,
        })
        .collect();

    let prices = state.spot_prices.lock().map_err(|_| {
        tracing::error!("settlement preview spot-price lock poisoned");
        StatusCode::INTERNAL_SERVER_ERROR
    })?.clone();
    let vols = state.vol_surface.lock().map_err(|_| {
        tracing::error!("settlement preview volatility lock poisoned");
        StatusCode::INTERNAL_SERVER_ERROR
    })?.clone();
    let current_margin = margin_required(&positions, &prices, &vols);
    let mut cash_delta = 0.0;
    let mut collateral_delta = 0.0;
    let mut spot_for_response = None;
    let mut premium_for_response = None;
    let mut violations = Vec::new();

    match request.action {
        SettlementAction::Deposit | SettlementAction::Withdraw => {
            let amount = request.amount.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            if !amount.is_finite() || amount <= 0.0 {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            cash_delta = if matches!(request.action, SettlementAction::Deposit) {
                amount
            } else {
                -amount
            };
        }
        SettlementAction::Open | SettlementAction::Substitution => {
            if matches!(request.action, SettlementAction::Substitution) {
                let old = close_position(&mut positions, request.position_id.as_deref())?;
                cash_delta += close_cashflow(&old, &prices, &vols)?.1;
                collateral_delta -= old.collateral;
            }
            let underlying = request
                .underlying
                .as_deref()
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let strike = request.strike.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let expiry_days = request.expiry_days.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let option_type = request
                .option_type
                .as_deref()
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let position_type = request
                .position_type
                .as_deref()
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let contracts = request.contracts.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            if !strike.is_finite()
                || strike <= 0.0
                || !expiry_days.is_finite()
                || expiry_days <= 0.0
                || !contracts.is_finite()
                || contracts <= 0.0
                || !matches!(option_type, "call" | "put")
                || !matches!(position_type, "long" | "short")
            {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            let spot = *prices.get(underlying).ok_or(StatusCode::NOT_FOUND)?;
            let base_vol = *vols.get(underlying).ok_or(StatusCode::NOT_FOUND)?;
            let vol = smile_vol(base_vol, strike / spot);
            let premium = black_scholes(&BSInputs {
                spot,
                strike,
                vol,
                t: expiry_days / 365.0,
                r: 0.05,
                is_call: option_type == "call",
            })
            .premium;
            let collateral = if position_type == "short" {
                if option_type == "call" {
                    spot * contracts
                } else {
                    strike * 1.10 * contracts
                }
            } else {
                0.0
            };
            cash_delta += if position_type == "long" {
                -premium * contracts
            } else {
                premium * contracts
            };
            collateral_delta += collateral;
            spot_for_response = Some(spot);
            premium_for_response = Some(premium);
            positions.push(PositionRow {
                id: String::new(),
                underlying: underlying.to_string(),
                strike,
                expiry_days,
                option_type: option_type.to_string(),
                position_type: position_type.to_string(),
                contracts,
                entry_premium: premium,
                collateral,
            });
        }
        SettlementAction::Close
        | SettlementAction::Exercise
        | SettlementAction::AssignedSettlement => {
            let position = close_position(&mut positions, request.position_id.as_deref())?;
            let spot = *prices
                .get(&position.underlying)
                .ok_or(StatusCode::NOT_FOUND)?;
            let intrinsic = if position.option_type == "call" {
                (spot - position.strike).max(0.0)
            } else {
                (position.strike - spot).max(0.0)
            };
            let signed = if position.position_type == "short" {
                -1.0
            } else {
                1.0
            };
            cash_delta = if matches!(request.action, SettlementAction::Close) {
                let (premium, flow) = close_cashflow(&position, &prices, &vols)?;
                premium_for_response = Some(premium);
                flow
            } else {
                signed * intrinsic * position.contracts.abs()
            };
            collateral_delta = -position.collateral;
            spot_for_response = Some(spot);
            if premium_for_response.is_none() {
                premium_for_response = Some(intrinsic);
            }
        }
    }

    let post_action_margin = margin_required(&positions, &prices, &vols);
    let equity = balance + collateral_locked + cash_delta;
    if !equity.is_finite() || equity < 0.0 {
        violations.push("insufficient_account_equity".to_string());
    }
    if post_action_margin.is_none() || current_margin.is_none() {
        violations.push("margin_unavailable_for_open_positions".to_string());
    } else if post_action_margin.is_some_and(|margin| margin > equity) {
        violations.push("initial_margin_exceeds_account_equity".to_string());
    }
    let estimates_unavailable = vec![
        "fee schedule is not configured in the current API".to_string(),
        "chain gas/resource simulation is not available in the current backend".to_string(),
    ];

    Ok(Json(SettlementPreview {
        action: request.action,
        spot: spot_for_response,
        option_premium: premium_for_response,
        cash_delta,
        collateral_delta,
        current_margin,
        post_action_margin,
        estimated_fee: None,
        estimated_gas_units: None,
        violations,
        estimates_unavailable,
    }))
}

fn close_position(
    positions: &mut Vec<PositionRow>,
    position_id: Option<&str>,
) -> Result<PositionRow, StatusCode> {
    let position_id = position_id.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let index = positions
        .iter()
        .position(|position| position.id == position_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(positions.remove(index))
}

fn close_cashflow(
    position: &PositionRow,
    prices: &std::collections::HashMap<String, f64>,
    vols: &std::collections::HashMap<String, f64>,
) -> Result<(f64, f64), StatusCode> {
    let spot = *prices
        .get(&position.underlying)
        .ok_or(StatusCode::NOT_FOUND)?;
    let base_vol = *vols
        .get(&position.underlying)
        .ok_or(StatusCode::NOT_FOUND)?;
    let vol = smile_vol(base_vol, position.strike / spot);
    let premium = black_scholes(&BSInputs {
        spot,
        strike: position.strike,
        vol,
        t: position.expiry_days / 365.0,
        r: 0.05,
        is_call: position.option_type == "call",
    })
    .premium;
    let side = if position.position_type == "short" {
        -1.0
    } else {
        1.0
    };
    Ok((premium, -side * premium * position.contracts.abs()))
}

fn margin_required(
    positions: &[PositionRow],
    prices: &std::collections::HashMap<String, f64>,
    vols: &std::collections::HashMap<String, f64>,
) -> Option<f64> {
    const SPOT_SHOCKS: [f64; 7] = [-0.30, -0.15, -0.05, 0.0, 0.05, 0.15, 1.0];
    const VOL_SHOCKS: [f64; 3] = [-0.20, 0.0, 0.20];

    let marks = positions
        .iter()
        .map(|position| {
            let spot = *prices.get(&position.underlying)?;
            let base_vol = *vols.get(&position.underlying)?;
            let vol = smile_vol(base_vol, position.strike / spot);
            let signed_contracts = position.contracts
                * if position.position_type == "short" { -1.0 } else { 1.0 };
            let base = black_scholes(&BSInputs {
                spot,
                strike: position.strike,
                vol,
                t: position.expiry_days / 365.0,
                r: 0.05,
                is_call: position.option_type == "call",
            })
            .premium;
            Some((spot, vol, signed_contracts, base))
        })
        .collect::<Option<Vec<_>>>()?;
    let mut worst_loss: f64 = 0.0;
    for spot_shock in SPOT_SHOCKS {
        for vol_shock in VOL_SHOCKS {
            let mut pnl = 0.0;
            for position in positions.iter().zip(&marks) {
                let (position, (spot, vol, signed_contracts, base)) = position;
                let shocked = black_scholes(&BSInputs {
                    spot: spot * (1.0 + spot_shock),
                    strike: position.strike,
                    vol: (vol + vol_shock).max(0.0),
                    t: position.expiry_days / 365.0,
                    r: 0.05,
                    is_call: position.option_type == "call",
                })
                .premium;
                pnl += (shocked - base) * signed_contracts;
            }
            if !pnl.is_finite() {
                return None;
            }
            worst_loss = worst_loss.max(-pnl);
        }
    }
    worst_loss.is_finite().then_some(worst_loss)
}
