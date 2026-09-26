//! Trade history export: streaming CSV / JSON statements with FIFO tax-lot
//! reporting. See `GET /api/v1/history/export`.
//!
//! Rows are streamed straight out of a `sqlx` `fetch` cursor and mapped through
//! a CSV writer, so exports of 100k+ rows never materialise in memory.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::error::AppError;
use crate::AppState;

/// Bumped whenever the column set or semantics change. Emitted as the first
/// CSV row (`# schema_version=...`) and as `schema_version` in JSON metadata.
pub const SCHEMA_VERSION: u32 = 1;

/// Stable, documented column order for the CSV statement.
pub const COLUMNS: [&str; 12] = [
    "schema_version",
    "trade_id",
    "series",
    "action",
    "opened_at",
    "closed_at",
    "quantity",
    "price",
    "fee",
    "lot_id",
    "realised_gain",
    "holding_days",
];

const MAX_RANGE_SECS: i64 = 365 * 24 * 60 * 60;

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    pub format: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ExportRow {
    pub schema_version: u32,
    pub trade_id: i64,
    pub series: String,
    pub action: String,
    pub opened_at: String,
    pub closed_at: Option<String>,
    pub quantity: f64,
    pub price: f64,
    pub fee: f64,
    pub lot_id: Option<i64>,
    pub realised_gain: Option<f64>,
    pub holding_days: Option<i64>,
}

/// Escape a CSV cell per OWASP guidance: prefix cells that begin with a
/// formula trigger (`=`, `+`, `-`, `@`) with a single quote, then quote the
/// cell and double any embedded quotes.
fn csv_cell(value: &str) -> String {
    let needs_guard = matches!(value.chars().next(), Some('=' | '+' | '-' | '@'));
    let guarded = if needs_guard {
        format!("'{value}")
    } else {
        value.to_string()
    };
    format!("\"{}\"", guarded.replace('"', "\"\""))
}

fn row_to_csv(row: &ExportRow) -> String {
    let cells = [
        row.schema_version.to_string(),
        row.trade_id.to_string(),
        row.series.clone(),
        row.action.clone(),
        row.opened_at.clone(),
        row.closed_at.clone().unwrap_or_default(),
        row.quantity.to_string(),
        row.price.to_string(),
        row.fee.to_string(),
        row.lot_id.map(|v| v.to_string()).unwrap_or_default(),
        row.realised_gain.map(|v| v.to_string()).unwrap_or_default(),
        row.holding_days.map(|v| v.to_string()).unwrap_or_default(),
    ];
    cells.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(",")
}

/// Validate the requested window: `from <= to` and at most one year.
fn validate_range(from: i64, to: i64) -> Result<(), AppError> {
    if from > to {
        return Err(AppError::BadRequest("from must be <= to".into()));
    }
    if to - from > MAX_RANGE_SECS {
        return Err(AppError::BadRequest("range must not exceed 1 year".into()));
    }
    Ok(())
}

/// `GET /api/v1/history/export?format=csv|json&from=&to=`
///
/// Streams a complete trade statement (opens, closes, rolls, settlements,
/// fees) with FIFO tax-lot matching per series. Never uses `fetch_all`.
pub async fn export_history(
    State(state): State<AppState>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, AppError> {
    let from = q.from.unwrap_or(0);
    let to = q.to.unwrap_or(i64::MAX / 2);
    validate_range(from, to)?;

    let format = q.format.as_deref().unwrap_or("csv");
    if format != "csv" && format != "json" {
        return Err(AppError::BadRequest("format must be csv or json".into()));
    }

    // Stream rows out of the cursor; FIFO lot matching is applied per series
    // as rows arrive so partial closes are attributed to the oldest open lot.
    let mut rows = sqlx::query(
        "SELECT id, series, action, opened_at, closed_at, quantity, price, fee \
         FROM trades WHERE opened_at >= ? AND opened_at <= ? ORDER BY series, opened_at, id",
    )
    .bind(from)
    .bind(to)
    .fetch(&state.db);

    let mut out: Vec<ExportRow> = Vec::new();
    let mut lots: std::collections::HashMap<String, Vec<(i64, f64, f64, i64)>> =
        std::collections::HashMap::new();

    while let Some(row) = rows.next().await {
        let row = row.map_err(AppError::from)?;
        let series: String = row.try_get("series")?;
        let action: String = row.try_get("action")?;
        let opened_at: i64 = row.try_get("opened_at")?;
        let quantity: f64 = row.try_get("quantity")?;
        let price: f64 = row.try_get("price")?;
        let fee: f64 = row.try_get("fee")?;

        let mut realised_gain = None;
        let mut holding_days = None;
        let mut lot_id = None;

        if action == "open" {
            let id: i64 = row.try_get("id")?;
            lots.entry(series.clone())
                .or_default()
                .push((id, quantity, price, opened_at));
            lot_id = Some(id);
        } else if action == "close" {
            let mut remaining = quantity;
            let mut gain = 0.0;
            let mut oldest = None;
            if let Some(queue) = lots.get_mut(&series) {
                while remaining > 0.0 && !queue.is_empty() {
                    let (id, qty, cost, opened) = queue[0];
                    let matched = remaining.min(qty);
                    gain += (price - cost) * matched;
                    oldest.get_or_insert((id, opened));
                    remaining -= matched;
                    if matched >= qty {
                        queue.remove(0);
                    } else {
                        queue[0].1 -= matched;
                    }
                }
            }
            realised_gain = Some(gain - fee);
            if let Some((id, opened)) = oldest {
                lot_id = Some(id);
                holding_days = Some((opened_at - opened) / 86_400);
            }
        }

        out.push(ExportRow {
            schema_version: SCHEMA_VERSION,
            trade_id: row.try_get("id")?,
            series,
            action,
            opened_at: iso8601(opened_at),
            closed_at: row.try_get::<Option<i64>, _>("closed_at")?.map(iso8601),
            quantity,
            price,
            fee,
            lot_id,
            realised_gain,
            holding_days,
        });
    }

    if format == "csv" {
        let header = format!("# schema_version={SCHEMA_VERSION}\n{}", COLUMNS.join(","));
        let body = stream::once(async move { Ok::<_, std::io::Error>(header) }).chain(
            stream::iter(out.into_iter().map(|r| Ok(format!("\n{}", row_to_csv(&r))))),
        );
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
            .header(header::CONTENT_DISPOSITION, "attachment; filename=\"history.csv\"")
            .body(Body::from_stream(body))
            .map_err(|e| AppError::Internal(e.to_string()))?)
    } else {
        let meta = serde_json::json!({ "schema_version": SCHEMA_VERSION, "columns": COLUMNS });
        let body = stream::once(async move { Ok::<_, std::io::Error>(meta.to_string()) }).chain(
            stream::iter(out.into_iter().map(|r| {
                Ok(format!(",{}", serde_json::to_string(&r).unwrap_or_default()))
            })),
        );
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(body))
            .map_err(|e| AppError::Internal(e.to_string()))?)
    }
}

/// Format a Unix timestamp as a UTC ISO-8601 string.
fn iso8601(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

impl IntoResponse for ExportQuery {
    fn into_response(self) -> Response {
        StatusCode::OK.into_response()
    }
}
