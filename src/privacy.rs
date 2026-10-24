use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};

use crate::AppState;

#[derive(Serialize, FromRow)]
struct AccountExport {
    wallet_address: String,
    balance: f64,
    collateral_locked: f64,
    created_at: String,
}

#[derive(Serialize, FromRow)]
struct PositionExport {
    id: String,
    underlying: String,
    strike: f64,
    expiry_days: f64,
    option_type: String,
    position_type: String,
    contracts: f64,
    entry_premium: f64,
    entry_spot: f64,
    collateral: f64,
    status: String,
    close_premium: Option<f64>,
    close_spot: Option<f64>,
    realized_pnl: Option<f64>,
    opened_at: String,
    closed_at: Option<String>,
}

#[derive(Serialize, FromRow)]
struct SettlementExport {
    id: String,
    event_type: String,
    occurred_at: i64,
    series_id: Option<String>,
    amount_stroops: i64,
    chain_tx_hash: Option<String>,
    description: String,
}

async fn session_wallet(pool: &SqlitePool, headers: &HeaderMap) -> Result<String, StatusCode> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let wallet = sqlx::query_scalar(
        "SELECT wallet_address FROM sessions
          WHERE token = ?1 AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(token)
    .fetch_optional(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(wallet)
}

/// Return the authenticated wallet's account and complete position history.
pub async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let wallet = session_wallet(&state.db, &headers).await?;
    let account: Option<AccountExport> = sqlx::query_as(
        "SELECT wallet_address, balance, collateral_locked, created_at
           FROM accounts WHERE wallet_address = ?1",
    )
    .bind(&wallet)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let positions: Vec<PositionExport> = sqlx::query_as(
        "SELECT id, underlying, strike, expiry_days, option_type, position_type,
                contracts, entry_premium, entry_spot, collateral, status,
                close_premium, close_spot, realized_pnl, opened_at, closed_at
           FROM positions WHERE wallet_address = ?1
          ORDER BY opened_at, id",
    )
    .bind(&wallet)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let settlements: Vec<SettlementExport> = sqlx::query_as(
        "SELECT id, event_type, occurred_at, series_id, amount_stroops,
                chain_tx_hash, description
           FROM settlement_activity WHERE wallet_address = ?1
          ORDER BY occurred_at, id",
    )
    .bind(&wallet)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({
        "schema_version": 1,
        "wallet_anchor": wallet,
        "account": account,
        "positions": positions,
        "settlement_activity": settlements,
    })))
}

/// Remove off-chain authentication and delivery data while retaining the
/// account, positions, and public wallet anchor needed for financial audit.
pub async fn erase(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let wallet = session_wallet(&state.db, &headers).await?;
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let unsettled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM positions
                        WHERE wallet_address = ?1 AND status = 'open')",
    )
    .bind(&wallet)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if unsettled {
        return Err(StatusCode::CONFLICT);
    }

    sqlx::query(
        "INSERT INTO privacy_erasure_log (wallet_anchor, erased_at)
         VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
    )
    .bind(&wallet)
    .execute(&mut *tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    sqlx::query("DELETE FROM sessions WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("DELETE FROM auth_nonces WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("DELETE FROM api_keys WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("DELETE FROM watchlist WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("DELETE FROM alerts WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query(
        "DELETE FROM delivery_attempts
          WHERE event_id IN (SELECT id FROM delivery_events WHERE wallet_address = ?1)",
    )
    .bind(&wallet)
    .execute(&mut *tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("DELETE FROM delivery_events WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("DELETE FROM delivery_channels WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    sqlx::query("DELETE FROM webhook_endpoints WHERE wallet_address = ?1")
        .bind(&wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tx.commit()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"erased": true, "wallet_anchor_retained": true})),
    ))
}
