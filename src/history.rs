use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{AppError, AppQuery};
use crate::models::Position;
use crate::positions::{DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT};
use crate::AppState;

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Serialize)]
pub struct HistoryStats {
    pub trade_count: i64,
    pub win_count: i64,
    pub loss_count: i64,
    pub total_realized_pnl: f64,
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub trades: Vec<Position>,
    pub stats: HistoryStats,
    /// Whether requesting the next `offset` would return more trades.
    /// `stats.trade_count` already IS the total across all pages, so
    /// unlike list_positions this doesn't need a separate response
    /// header — it's just another field on an already-object-shaped body.
    pub has_more: bool,
}

/// The trade ledger is just closed/rolled rows from `positions` — there's
/// no separate append-only history table, since a position's own status
/// transition already records everything a ledger entry needs.
///
/// `stats` is always computed over the FULL history regardless of
/// limit/offset — pagination only applies to which rows `trades` returns,
/// since a win/loss/pnl summary that changed depending on which page you
/// requested would be actively misleading.
pub async fn get_history(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    AppQuery(q): AppQuery<HistoryQuery>,
) -> Result<Json<HistoryResponse>, AppError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let offset = q.offset.unwrap_or(0).max(0);

    let trades: Vec<Position> = sqlx::query_as(
        "SELECT * FROM positions
            WHERE wallet_address = ? AND status IN ('closed', 'rolled')
         ORDER BY closed_at DESC
         LIMIT ? OFFSET ?",
    )
    .bind(&wallet_address)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let (trade_count, win_count, loss_count, total_realized_pnl): (i64, i64, i64, Option<f64>) =
        sqlx::query_as(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN realized_pnl > 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN realized_pnl < 0 THEN 1 ELSE 0 END), 0),
                SUM(realized_pnl)
             FROM positions
             WHERE wallet_address = ? AND status IN ('closed', 'rolled')",
        )
        .bind(&wallet_address)
        .fetch_one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let has_more = offset + (trades.len() as i64) < trade_count;
    let stats = HistoryStats {
        trade_count,
        win_count,
        loss_count,
        total_realized_pnl: total_realized_pnl.unwrap_or(0.0),
    };

    Ok(Json(HistoryResponse {
        trades,
        stats,
        has_more,
    }))
}
