use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

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

pub async fn open_position(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Json(req): Json<OpenPositionRequest>,
) -> Result<Json<Position>, StatusCode> {
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

    let mut tx = state.db.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(&wallet_address)
        .fetch_one(&mut *tx)
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
        .bind(&wallet_address)
        .execute(&mut *tx)
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
    .bind(&wallet_address)
    .bind(&req.underlying)
    .bind(req.strike)
    .bind(req.expiry_days)
    .bind(&req.option_type)
    .bind(&req.position_type)
    .bind(req.contracts)
    .bind(entry_premium)
    .bind(spot)
    .bind(collateral)
    .execute(&mut *tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let position: Position = sqlx::query_as("SELECT * FROM positions WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(position))
}
