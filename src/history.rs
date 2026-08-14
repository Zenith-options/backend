use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Serialize;

use crate::auth::AuthUser;
use crate::error::AppError;
use crate::models::Position;
use crate::AppState;

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
}

/// The trade ledger is just closed/rolled rows from `positions` — there's
/// no separate append-only history table, since a position's own status
/// transition already records everything a ledger entry needs.
pub async fn get_history(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<HistoryResponse>, AppError> {
    let trades: Vec<Position> = sqlx::query_as(
        "SELECT * FROM positions
            WHERE wallet_address = ? AND status IN ('closed', 'rolled')
         ORDER BY closed_at DESC",
    )
    .bind(&wallet_address)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut win_count = 0;
    let mut loss_count = 0;
    let mut total_realized_pnl = 0.0;
    for trade in &trades {
        let pnl = trade.realized_pnl.unwrap_or(0.0);
        total_realized_pnl += pnl;
        if pnl > 0.0 {
            win_count += 1;
        } else if pnl < 0.0 {
            loss_count += 1;
        }
    }

    let stats = HistoryStats {
        trade_count: trades.len() as i64,
        win_count,
        loss_count,
        total_realized_pnl,
    };

    Ok(Json(HistoryResponse { trades, stats }))
}
