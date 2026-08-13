use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, Transaction};

use crate::auth::AuthUser;
use crate::collateral::collateral_required;
use crate::models::{Account, Position};
use crate::{black_scholes, smile_vol, AppState, BSInputs};

pub async fn get_account(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Account>, StatusCode> {
    // Verify/login already creates this row, but stay defensive in case a
    // session outlives some future account-deletion path.
    sqlx::query(
        "INSERT INTO accounts (wallet_address) VALUES (?) ON CONFLICT(wallet_address) DO NOTHING",
    )
    .bind(&wallet_address)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(&wallet_address)
        .fetch_one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(account))
}

#[derive(Deserialize)]
pub struct ListPositionsQuery {
    /// "open" | "closed" | "rolled" — omit to return every status.
    pub status: Option<String>,
}

pub async fn list_positions(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Query(q): Query<ListPositionsQuery>,
) -> Result<Json<Vec<Position>>, StatusCode> {
    let positions: Vec<Position> = match q.status {
        Some(status) => {
            sqlx::query_as(
                "SELECT * FROM positions WHERE wallet_address = ? AND status = ? ORDER BY opened_at DESC",
            )
            .bind(&wallet_address)
            .bind(&status)
            .fetch_all(&state.db)
            .await
        }
        None => {
            sqlx::query_as("SELECT * FROM positions WHERE wallet_address = ? ORDER BY opened_at DESC")
                .bind(&wallet_address)
                .fetch_all(&state.db)
                .await
        }
    }
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(positions))
}

#[derive(Deserialize)]
pub struct OpenPositionRequest {
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String,   // "call" | "put"
    pub position_type: String, // "long" | "short"
    pub contracts: f64,
}

/// Prices and inserts a new position, debiting/crediting the account and
/// locking collateral as needed, all within the caller's transaction.
/// Shared by the open handler and (once it exists) the roll handler, so
/// rolling a position doesn't need to duplicate this logic.
async fn open_position_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    state: &AppState,
    wallet_address: &str,
    req: &OpenPositionRequest,
) -> Result<Position, StatusCode> {
    if req.contracts <= 0.0 || req.strike <= 0.0 || req.expiry_days <= 0.0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if req.option_type != "call" && req.option_type != "put" {
        return Err(StatusCode::BAD_REQUEST);
    }
    if req.position_type != "long" && req.position_type != "short" {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (spot, base_vol) = {
        let prices = state.spot_prices.lock().unwrap();
        let vols = state.vol_surface.lock().unwrap();
        let spot = *prices.get(&req.underlying).ok_or(StatusCode::NOT_FOUND)?;
        let vol = *vols.get(&req.underlying).ok_or(StatusCode::NOT_FOUND)?;
        (spot, vol)
    };

    let vol = smile_vol(base_vol, req.strike / spot);
    let t = req.expiry_days / 365.0;
    let is_call = req.option_type == "call";
    let entry_premium = black_scholes(&BSInputs {
        spot,
        strike: req.strike,
        vol,
        t,
        r: 0.05,
        is_call,
    })
    .premium;

    let is_short = req.position_type == "short";
    let collateral = if is_short {
        collateral_required(&req.option_type, req.contracts, req.strike, spot)
    } else {
        0.0
    };
    let cash_delta = if is_short {
        entry_premium * req.contracts // premium received
    } else {
        -entry_premium * req.contracts // premium paid
    };

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(wallet_address)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let new_balance = account.balance + cash_delta;
    let new_collateral_locked = account.collateral_locked + collateral;
    // Available buying power must stay non-negative: cash on hand minus
    // whatever's locked as collateral (across all positions, not just
    // this one) must cover this trade's premium debit/collateral.
    if new_balance - new_collateral_locked < 0.0 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    sqlx::query("UPDATE accounts SET balance = ?, collateral_locked = ? WHERE wallet_address = ?")
        .bind(new_balance)
        .bind(new_collateral_locked)
        .bind(wallet_address)
        .execute(&mut **tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO positions
            (id, wallet_address, underlying, strike, expiry_days, option_type,
             position_type, contracts, entry_premium, entry_spot, collateral, status)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open')",
    )
    .bind(&id)
    .bind(wallet_address)
    .bind(&req.underlying)
    .bind(req.strike)
    .bind(req.expiry_days)
    .bind(&req.option_type)
    .bind(&req.position_type)
    .bind(req.contracts)
    .bind(entry_premium)
    .bind(spot)
    .bind(collateral)
    .execute(&mut **tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let position: Position = sqlx::query_as("SELECT * FROM positions WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(position)
}

pub async fn open_position(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Json(req): Json<OpenPositionRequest>,
) -> Result<Json<Position>, StatusCode> {
    let mut tx = state.db.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let position = open_position_in_tx(&mut tx, &state, &wallet_address, &req).await?;
    tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(position))
}

/// Reprices an open position at the current spot/vol and settles it:
/// releases any locked collateral, applies the closing cash flow to the
/// account, and marks the row closed. Shared by the close and roll
/// handlers so both settle a position the same way.
///
/// Simplification: this reprices with the *same* time-to-expiry the
/// position was opened with rather than tracking real elapsed time
/// against an absolute expiry timestamp — fine for a paper-trading demo,
/// but not a real theta decay model.
async fn close_position_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    state: &AppState,
    wallet_address: &str,
    position_id: &str,
) -> Result<Position, StatusCode> {
    let position: Position = sqlx::query_as(
        "SELECT * FROM positions WHERE id = ? AND wallet_address = ? AND status = 'open'",
    )
    .bind(position_id)
    .bind(wallet_address)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;

    let (spot, base_vol) = {
        let prices = state.spot_prices.lock().unwrap();
        let vols = state.vol_surface.lock().unwrap();
        let spot = *prices.get(&position.underlying).ok_or(StatusCode::NOT_FOUND)?;
        let vol = *vols.get(&position.underlying).ok_or(StatusCode::NOT_FOUND)?;
        (spot, vol)
    };

    let vol = smile_vol(base_vol, position.strike / spot);
    let t = position.expiry_days / 365.0;
    let is_call = position.option_type == "call";
    let close_premium = black_scholes(&BSInputs { spot, strike: position.strike, vol, t, r: 0.05, is_call }).premium;

    let is_short = position.position_type == "short";
    let realized_pnl = if is_short {
        (position.entry_premium - close_premium) * position.contracts
    } else {
        (close_premium - position.entry_premium) * position.contracts
    };
    let cash_delta = if is_short {
        -close_premium * position.contracts // buy to close
    } else {
        close_premium * position.contracts // sell to close
    };

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(wallet_address)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let new_balance = account.balance + cash_delta;
    let new_collateral_locked = account.collateral_locked - position.collateral;

    sqlx::query("UPDATE accounts SET balance = ?, collateral_locked = ? WHERE wallet_address = ?")
        .bind(new_balance)
        .bind(new_collateral_locked)
        .bind(wallet_address)
        .execute(&mut **tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    sqlx::query(
        "UPDATE positions
            SET status = 'closed', close_premium = ?, close_spot = ?, realized_pnl = ?,
                closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?",
    )
    .bind(close_premium)
    .bind(spot)
    .bind(realized_pnl)
    .bind(position_id)
    .execute(&mut **tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let closed: Position = sqlx::query_as("SELECT * FROM positions WHERE id = ?")
        .bind(position_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(closed)
}

pub async fn close_position(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Path(id): Path<String>,
) -> Result<Json<Position>, StatusCode> {
    let mut tx = state.db.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let closed = close_position_in_tx(&mut tx, &state, &wallet_address, &id).await?;
    tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(closed))
}

#[derive(Deserialize)]
pub struct RollPositionRequest {
    pub new_strike: f64,
    pub new_expiry_days: f64,
}

#[derive(serde::Serialize)]
pub struct RollResult {
    pub closed: Position,
    pub opened: Position,
}

/// Closes the given position and immediately opens its replacement (same
/// underlying/option_type/position_type/contracts, new strike and expiry)
/// as one atomic transaction — either both happen or neither does.
pub async fn roll_position(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<RollPositionRequest>,
) -> Result<Json<RollResult>, StatusCode> {
    let mut tx = state.db.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut closed = close_position_in_tx(&mut tx, &state, &wallet_address, &id).await?;
    // close_position_in_tx always marks the row 'closed'; a roll is
    // specifically a close-and-reopen, so relabel it 'rolled' to keep
    // /api/v1/history's ledger distinguishable from a plain close.
    sqlx::query("UPDATE positions SET status = 'rolled' WHERE id = ?")
        .bind(&closed.id)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    closed.status = "rolled".to_string();

    let open_req = OpenPositionRequest {
        underlying: closed.underlying.clone(),
        strike: req.new_strike,
        expiry_days: req.new_expiry_days,
        option_type: closed.option_type.clone(),
        position_type: closed.position_type.clone(),
        contracts: closed.contracts,
    };
    let opened = open_position_in_tx(&mut tx, &state, &wallet_address, &open_req).await?;

    tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(RollResult { closed, opened }))
}

#[derive(Serialize, Default)]
pub struct AggregateGreeks {
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
}

/// Sums each open position's current Greeks (repriced at today's
/// spot/vol, not the entry-time values stored on the row), flipping sign
/// for short positions — ported from the frontend's aggregateGreeks().
pub async fn get_portfolio_greeks(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<AggregateGreeks>, StatusCode> {
    let open_positions: Vec<Position> =
        sqlx::query_as("SELECT * FROM positions WHERE wallet_address = ? AND status = 'open'")
            .bind(&wallet_address)
            .fetch_all(&state.db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut totals = AggregateGreeks::default();
    for p in &open_positions {
        let (spot, base_vol) = {
            let prices = state.spot_prices.lock().unwrap();
            let vols = state.vol_surface.lock().unwrap();
            match (prices.get(&p.underlying), vols.get(&p.underlying)) {
                (Some(&s), Some(&v)) => (s, v),
                _ => continue, // underlying delisted since this position was opened
            }
        };

        let vol = smile_vol(base_vol, p.strike / spot);
        let t = p.expiry_days / 365.0;
        let is_call = p.option_type == "call";
        let result = black_scholes(&BSInputs { spot, strike: p.strike, vol, t, r: 0.05, is_call });

        let sign = if p.position_type == "short" { -1.0 } else { 1.0 };
        totals.delta += sign * result.delta * p.contracts;
        totals.gamma += sign * result.gamma * p.contracts;
        totals.theta += sign * result.theta * p.contracts;
        totals.vega += sign * result.vega * p.contracts;
    }

    Ok(Json(totals))
}
