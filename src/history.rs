use axum::extract::State;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{db_error, AppError, AppQuery};
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

/// Row shape returned by the stats query. `win_count`/`loss_count` are
/// `COALESCE(..., 0)` so they are never NULL; `total_realized_pnl` is a bare
/// `SUM`, which is NULL for an empty history, hence `Option`. The `!`/`_`
/// column overrides tell the macro how to decode the aggregate expressions,
/// whose types SQLite's planner cannot always prove.
struct HistoryStatsRow {
    trade_count: i64,
    win_count: i64,
    loss_count: i64,
    total_realized_pnl: Option<f64>,
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

    let trades: Vec<Position> = sqlx::query_as!(
        Position,
        "SELECT id AS \"id!\", wallet_address, underlying, strike, expiry_days, option_type, position_type, contracts, entry_premium, entry_spot, collateral, status, close_premium, close_spot, realized_pnl, opened_at, closed_at, strategy_id FROM positions WHERE wallet_address = ? AND status IN ('closed', 'rolled') ORDER BY closed_at DESC LIMIT ? OFFSET ?",
        &wallet_address,
        limit,
        offset
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("load trade history", e))?;

    let row = sqlx::query_as!(
        HistoryStatsRow,
        "SELECT COUNT(*) AS trade_count, COALESCE(SUM(CASE WHEN realized_pnl > 0 THEN 1 ELSE 0 END), 0) AS \"win_count!: i64\", COALESCE(SUM(CASE WHEN realized_pnl < 0 THEN 1 ELSE 0 END), 0) AS \"loss_count!: i64\", SUM(realized_pnl) AS \"total_realized_pnl: _\" FROM positions WHERE wallet_address = ? AND status IN ('closed', 'rolled')",
        &wallet_address
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| db_error("compute trade history stats", e))?;

    let has_more = offset + (trades.len() as i64) < row.trade_count;
    let stats = HistoryStats {
        trade_count: row.trade_count,
        win_count: row.win_count,
        loss_count: row.loss_count,
        total_realized_pnl: row.total_realized_pnl.unwrap_or(0.0),
    };

    Ok(Json(HistoryResponse {
        trades,
        stats,
        has_more,
    }))
}
