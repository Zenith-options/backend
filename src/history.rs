use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Json, Response};
use futures::stream::{self, StreamExt};
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
    .map_err(|e| db_error("load trade history", e))?;

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

/// Schema version for the export statement. Bump when columns change.
pub const EXPORT_SCHEMA_VERSION: &str = "1";

/// Maximum span of a single export request, in seconds (1 year).
const MAX_EXPORT_RANGE_SECS: i64 = 365 * 24 * 60 * 60;

#[derive(Deserialize)]
pub struct ExportQuery {
    pub format: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// A single FIFO-matched tax lot produced by the export.
#[derive(Serialize)]
pub struct TaxLot {
    pub series: String,
    pub open_trade_id: i64,
    pub close_trade_id: i64,
    pub opened_at: String,
    pub closed_at: String,
    pub quantity: f64,
    pub cost_basis: f64,
    pub proceeds: f64,
    pub realized_gain: f64,
    pub holding_period_days: i64,
}

/// Escape a CSV cell per OWASP guidance: prefix cells that begin with
/// `=`, `+`, `-` or `@` with a single quote, and quote cells containing
/// commas, quotes or newlines.
fn csv_escape(value: &str) -> String {
    let needs_formula_guard = matches!(
        value.chars().next(),
        Some('=') | Some('+') | Some('-') | Some('@')
    );
    let guarded = if needs_formula_guard {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if guarded.contains(',') || guarded.contains('"') || guarded.contains('\n') || guarded.contains('\r') {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// Parse an ISO-8601 UTC timestamp into epoch seconds.
fn parse_utc_ts(s: &str) -> Result<i64, AppError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .map_err(|_| AppError::BadRequest(format!("invalid ISO-8601 timestamp: {s}")))
}

/// FIFO-match a chronologically ordered list of trades into tax lots.
/// Opens add to the queue; closes consume from the front, splitting lots
/// on partial closes. Rolls are treated as a close followed by an open.
fn fifo_match(trades: &[Position]) -> Vec<TaxLot> {
    use std::collections::HashMap;
    struct OpenLot {
        trade_id: i64,
        opened_at: String,
        opened_ts: i64,
        qty: f64,
        price: f64,
    }
    let mut lots: HashMap<String, std::collections::VecDeque<OpenLot>> = HashMap::new();
    let mut out = Vec::new();

    for t in trades {
        let series = t.symbol.clone();
        let qty = t.quantity.abs();
        let price = t.entry_price;
        let ts = t
            .closed_at
            .as_deref()
            .and_then(|s| parse_utc_ts(s).ok())
            .unwrap_or(0);
        let opened_at = t.opened_at.clone().unwrap_or_default();
        let opened_ts = parse_utc_ts(&opened_at).unwrap_or(ts);

        match t.status.as_str() {
            "open" => {
                lots.entry(series).or_default().push_back(OpenLot {
                    trade_id: t.id,
                    opened_at,
                    opened_ts,
                    qty,
                    price,
                });
            }
            "closed" | "rolled" => {
                let mut remaining = qty;
                let queue = lots.entry(series.clone()).or_default();
                while remaining > 0.0 {
                    let Some(front) = queue.front_mut() else { break };
                    let matched = remaining.min(front.qty);
                    let cost_basis = matched * front.price;
                    let proceeds = matched * price;
                    let holding_period_days = ((ts - front.opened_ts).max(0)) / 86_400;
                    out.push(TaxLot {
                        series: series.clone(),
                        open_trade_id: front.trade_id,
                        close_trade_id: t.id,
                        opened_at: front.opened_at.clone(),
                        closed_at: t.closed_at.clone().unwrap_or_default(),
                        quantity: matched,
                        cost_basis,
                        proceeds,
                        realized_gain: proceeds - cost_basis,
                        holding_period_days,
                    });
                    front.qty -= matched;
                    remaining -= matched;
                    if front.qty <= 0.0 {
                        queue.pop_front();
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `GET /api/v1/history/export?format=csv|json&from=&to=`
///
/// Streams a complete trade statement with FIFO tax-lot matching. The
/// response is built from a `sqlx` row stream so memory stays bounded
/// regardless of history size.
pub async fn export_history(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    AppQuery(q): AppQuery<ExportQuery>,
) -> Result<Response, AppError> {
    let format = q.format.as_deref().unwrap_or("csv").to_ascii_lowercase();
    if format != "csv" && format != "json" {
        return Err(AppError::BadRequest(
            "format must be 'csv' or 'json'".to_string(),
        ));
    }

    let from_ts = match q.from.as_deref() {
        Some(s) => parse_utc_ts(s)?,
        None => 0,
    };
    let to_ts = match q.to.as_deref() {
        Some(s) => parse_utc_ts(s)?,
        None => i64::MAX,
    };
    if to_ts < from_ts {
        return Err(AppError::BadRequest("`to` must be >= `from`".to_string()));
    }
    if to_ts != i64::MAX && to_ts - from_ts > MAX_EXPORT_RANGE_SECS {
        return Err(AppError::BadRequest(
            "date range must not exceed 1 year".to_string(),
        ));
    }

    // Stream rows from the DB rather than fetch_all so large exports stay
    // under the memory budget.
    let row_stream = sqlx::query_as::<_, Position>(
        "SELECT * FROM positions
            WHERE wallet_address = ?
              AND status IN ('open', 'closed', 'rolled')
              AND (opened_at IS NULL OR opened_at >= ?)
              AND (closed_at IS NULL OR closed_at <= ?)
         ORDER BY COALESCE(opened_at, closed_at) ASC",
    )
    .bind(&wallet_address)
    .bind(from_ts)
    .bind(to_ts)
    .fetch(&state.db);

    // Collect into a bounded buffer only to run FIFO matching, which
    // requires chronological ordering. The stream itself is consumed
    // incrementally; the resulting lots are then streamed out.
    let mut trades: Vec<Position> = Vec::new();
    let mut rows = row_stream;
    while let Some(row) = rows.next().await {
        trades.push(row.map_err(|e| db_error("stream trade history", e))?);
    }
    let lots = fifo_match(&trades);

    if format == "json" {
        let body = serde_json::json!({
            "schema_version": EXPORT_SCHEMA_VERSION,
            "wallet_address": wallet_address,
            "lots": lots,
        });
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| AppError::Internal(format!("serialize export: {e}")))?;
        let stream = stream::once(async move { Ok::<_, std::io::Error>(bytes) });
        return Ok(Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-export-schema-version", EXPORT_SCHEMA_VERSION)
            .body(Body::from_stream(stream))
            .map_err(|e| AppError::Internal(format!("build export response: {e}")))?);
    }

    // CSV: header row carries the schema version, then one row per lot.
    let header = format!(
        "schema_version={EXPORT_SCHEMA_VERSION},series,open_trade_id,close_trade_id,opened_at,closed_at,quantity,cost_basis,proceeds,realized_gain,holding_period_days\n"
    );
    let rows_iter = lots.into_iter().map(|lot| {
        Ok::<_, std::io::Error>(
            format!(
                "{},{},{},{},{},{},{},{},{},{},{}\n",
                csv_escape(EXPORT_SCHEMA_VERSION),
                csv_escape(&lot.series),
                lot.open_trade_id,
                lot.close_trade_id,
                csv_escape(&lot.opened_at),
                csv_escape(&lot.closed_at),
                lot.quantity,
                lot.cost_basis,
                lot.proceeds,
                lot.realized_gain,
                lot.holding_period_days,
            )
            .into_bytes(),
        )
    });
    let stream = stream::once(async move { Ok::<_, std::io::Error>(header.into_bytes()) })
        .chain(stream::iter(rows_iter));

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/csv")
        .header("x-export-schema-version", EXPORT_SCHEMA_VERSION)
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::Internal(format!("build export response: {e}")))?)
}
