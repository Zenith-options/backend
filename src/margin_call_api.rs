//! HTTP handlers for the margin-call lifecycle API.
//!
//! Exposes:
//!   GET  /api/v1/margin/calls          – open call + history for the authenticated wallet
//!   GET  /api/v1/margin/calls/open     – the single open call (or 204)
//!   GET  /api/v1/account/margin        – existing margin summary extended with call status
//!
//! These are read-only: the state machine in `margin_calls.rs` drives all
//! transitions; users cure calls by depositing collateral.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{db_error, AppError};
use crate::margin_calls::{self, MarginCall};
use crate::AppState;

/// Query parameters for listing margin calls.
#[derive(Deserialize)]
pub struct ListMarginCallsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Response for `GET /api/v1/margin/calls`.
#[derive(Serialize)]
pub struct MarginCallsResponse {
    pub calls: Vec<MarginCall>,
    pub open_call: Option<MarginCall>,
}

/// `GET /api/v1/margin/calls` — full history + current open call (if any).
pub async fn list_margin_calls(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Query(q): Query<ListMarginCallsQuery>,
) -> Result<Json<MarginCallsResponse>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let calls =
        margin_calls::list_calls(&state.db, &wallet_address, limit, offset)
            .await
            .map_err(|e| db_error("list margin calls", e))?;

    let open_call =
        margin_calls::get_open_call(&state.db, &wallet_address)
            .await
            .map_err(|e| db_error("get open margin call", e))?;

    Ok(Json(MarginCallsResponse { calls, open_call }))
}

/// `GET /api/v1/margin/calls/open` — just the open call (204 if none).
pub async fn get_open_margin_call(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<impl IntoResponse, AppError> {
    let call =
        margin_calls::get_open_call(&state.db, &wallet_address)
            .await
            .map_err(|e| db_error("get open margin call", e))?;

    match call {
        Some(c) => Ok((StatusCode::OK, Json(c)).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// Margin summary enriched with the current margin-call status, returned from
/// `GET /api/v1/account/margin`.
#[derive(Serialize)]
pub struct MarginSummary {
    pub wallet_address: String,
    /// Current equity in display units (10^7 → f64).
    pub equity: f64,
    /// Initial margin requirement in display units.
    pub initial_margin: f64,
    /// Maintenance margin requirement in display units.
    pub maintenance_margin: f64,
    /// "healthy" | "margin_call" | "liquidating"
    pub health: String,
    /// Open margin call, if one is active.
    pub open_call: Option<MarginCall>,
}

const SCALE: f64 = 1e7;

/// `GET /api/v1/account/margin` — margin summary with call status.
///
/// For now this reads from the same `accounts` table that the existing
/// `/api/v1/account` endpoint uses. The production margin engine will
/// replace the equity/margin fields; the call status is new.
pub async fn get_margin_summary(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<MarginSummary>, AppError> {
    // Load raw account row: collateral_locked is used as a proxy for equity
    // until the production margin engine supplies real figures.
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT collateral_locked, COALESCE(collateral_locked, 0) FROM accounts
          WHERE wallet_address = ?",
    )
    .bind(&wallet_address)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| db_error("load account for margin summary", e))?;

    let (collateral_locked, _) = row.unwrap_or((0, 0));

    // Placeholder margin figures until the production engine hooks in.
    // These match the StrategyBasedMargin model: initial = locked, maint = 75%.
    let equity = collateral_locked as f64 / SCALE;
    let initial_margin = equity;
    let maintenance_margin = equity * 0.75;

    let open_call =
        margin_calls::get_open_call(&state.db, &wallet_address)
            .await
            .map_err(|e| db_error("get open margin call", e))?;

    let health = match &open_call {
        Some(_) => "margin_call".to_string(),
        None => "healthy".to_string(),
    };

    Ok(Json(MarginSummary {
        wallet_address,
        equity,
        initial_margin,
        maintenance_margin,
        health,
        open_call,
    }))
}
