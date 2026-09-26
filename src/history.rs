use axum::extract::State;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{db_error, AppError, AppQuery};
use crate::models::Position;
use crate::pagination::{Cursor, CursorPage};
use crate::positions::{DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT};
use crate::AppState;

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i64>,
    /// Deprecated offset fallback, kept for one release. Ignored when a
    /// `cursor` is supplied.
    pub offset: Option<i64>,
    /// Opaque, HMAC-signed keyset cursor encoding `(closed_at, id)`.
    pub cursor: Option<String>,
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
    /// Whether requesting the next page would return more trades.
    /// `stats.trade_count` already IS the total across all pages, so
    /// unlike list_positions this doesn't need a separate response
    /// header — it's just another field on an already-object-shaped body.
    pub has_more: bool,
    /// Opaque keyset cursor for the next page, or `null` at the end.
    pub next_cursor: Option<String>,
}

/// The trade ledger is just closed/rolled rows from `positions` — there's
/// no separate append-only history table, since a position's own status
/// transition already records everything a ledger entry needs.
///
/// `stats` is always computed over the FULL history regardless of
/// limit/offset — pagination only applies to which rows `trades` returns,
/// since a win/loss/pnl summary that changed depending on which page you
/// requested would be actively misleading.
///
/// Paging uses keyset (cursor) pagination over `(closed_at DESC, id DESC)`,
/// backed by the composite index `(wallet_address, closed_at DESC, id DESC)`.
/// The cursor binds the wallet address and the status filter set into its
/// HMAC, so a cursor minted for a different filter set is rejected.
pub async fn get_history(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    AppQuery(q): AppQuery<HistoryQuery>,
) -> Result<Json<HistoryResponse>, AppError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);

    // The filter set is bound into the cursor HMAC so a cursor from a
    // different filter set cannot be replayed here.
    let filter_scope = format!("history:{}:closed,rolled", wallet_address);

    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::<HistoryCursorKey>::decode(
            raw,
            &state.cursor_secret,
            &filter_scope,
        )?),
        None => None,
    };

    // Fetch one extra row to detect whether a next page exists without a
    // separate COUNT query.
    let fetch_limit = limit + 1;

    let mut trades: Vec<Position> = match &cursor {
        Some(c) => sqlx::query_as(
            "SELECT * FROM positions
                WHERE wallet_address = ? AND status IN ('closed', 'rolled')
                  AND (closed_at < ? OR (closed_at = ? AND id < ?))
             ORDER BY closed_at DESC, id DESC
             LIMIT ?",
        )
        .bind(&wallet_address)
        .bind(c.sort_key)
        .bind(c.sort_key)
        .bind(c.id)
        .bind(fetch_limit)
        .fetch_all(&state.db)
        .await
        .map_err(|e| db_error("load trade history", e))?,
        None => {
            // Deprecated offset fallback for one release: only used when no
            // cursor is supplied and an explicit offset is requested.
            let offset = q.offset.unwrap_or(0).max(0);
            sqlx::query_as(
                "SELECT * FROM positions
                    WHERE wallet_address = ? AND status IN ('closed', 'rolled')
                 ORDER BY closed_at DESC, id DESC
                 LIMIT ? OFFSET ?",
            )
            .bind(&wallet_address)
            .bind(fetch_limit)
            .bind(offset)
            .fetch_all(&state.db)
            .await
            .map_err(|e| db_error("load trade history", e))?
        }
    };

    let has_more = trades.len() as i64 > limit;
    if has_more {
        trades.truncate(limit as usize);
    }

    let next_cursor = if has_more {
        trades.last().map(|last| {
            Cursor::new(
                HistoryCursorKey {
                    sort_key: last.closed_at,
                    id: last.id,
                },
                &state.cursor_secret,
                &filter_scope,
            )
            .encode()
        })
    } else {
        None
    };

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
        .map_err(|e| db_error("compute trade history stats", e))?;

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
        next_cursor,
    }))
}

/// Sort key for the trade-history keyset cursor: `(closed_at, id)`.
#[derive(Serialize, Deserialize)]
struct HistoryCursorKey {
    sort_key: String,
    id: i64,
}
