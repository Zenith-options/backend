use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;
use sqlx::{FromRow, SqlitePool};

use crate::AppState;

const MAX_RANGE_SECS: i64 = 366 * 24 * 60 * 60;
const MAX_STATEMENT_RECORDS: usize = 50;

#[derive(Debug, Deserialize)]
pub struct StatementQuery {
    pub from: i64,
    pub to: i64,
    pub format: Option<String>,
    pub record_id: Option<String>,
}

#[derive(Debug, FromRow)]
pub struct StatementRecord {
    pub id: String,
    pub event_type: String,
    pub occurred_at: i64,
    pub series_id: Option<String>,
    pub amount_stroops: i64,
    pub chain_tx_hash: Option<String>,
    pub description: String,
}

async fn authenticated_wallet(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<String, StatusCode> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    sqlx::query_scalar(
        "SELECT wallet_address FROM sessions
          WHERE token = ?1 AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(token)
    .fetch_optional(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::UNAUTHORIZED)
}

async fn records(
    pool: &SqlitePool,
    wallet: &str,
    query: &StatementQuery,
) -> Result<Vec<StatementRecord>, StatusCode> {
    if query.from > query.to || query.to.saturating_sub(query.from) > MAX_RANGE_SECS {
        return Err(StatusCode::BAD_REQUEST);
    }
    let rows = sqlx::query_as(
        "SELECT id, event_type, occurred_at, series_id, amount_stroops,
                chain_tx_hash, description
           FROM settlement_activity
          WHERE wallet_address = ?1
            AND occurred_at >= ?2 AND occurred_at <= ?3
            AND (?4 IS NULL OR id = ?4)
          ORDER BY occurred_at, id",
    )
    .bind(wallet)
    .bind(query.from)
    .bind(query.to)
    .bind(query.record_id.as_deref())
    .fetch_all(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if rows.len() > MAX_STATEMENT_RECORDS {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    Ok(rows)
}

fn csv_cell(value: &str) -> String {
    let guarded = if matches!(value.chars().next(), Some('=' | '+' | '-' | '@')) {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    format!("\"{}\"", guarded.replace('"', "\"\""))
}

fn csv(records: &[StatementRecord]) -> Vec<u8> {
    let mut output = String::from(
        "record_id,event_type,occurred_at_unix,series_id,amount_stroops,running_period_balance_stroops,chain_tx_hash,description\n",
    );
    let mut running_balance = 0i64;
    for row in records {
        running_balance = running_balance.saturating_add(row.amount_stroops);
        let cells = [
            row.id.clone(),
            row.event_type.clone(),
            row.occurred_at.to_string(),
            row.series_id.clone().unwrap_or_default(),
            row.amount_stroops.to_string(),
            running_balance.to_string(),
            row.chain_tx_hash.clone().unwrap_or_default(),
            row.description.clone(),
        ];
        output.push_str(
            &cells
                .iter()
                .map(|value| csv_cell(value))
                .collect::<Vec<_>>()
                .join(","),
        );
        output.push('\n');
    }
    output.into_bytes()
}

fn pdf_text(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|ch| if (' '..='~').contains(&ch) { ch } else { '?' })
        .collect();
    safe.replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

fn pdf(records: &[StatementRecord], from: i64, to: i64) -> Vec<u8> {
    let mut commands = format!(
        "BT /F1 10 Tf 50 760 Td (Settlement statement: {from} to {to}) Tj 0 -18 Td "
    );
    if records.is_empty() {
        commands.push_str("(No settlement activity in this period) Tj ");
    } else {
        let mut running_balance = 0i64;
        for row in records {
            running_balance = running_balance.saturating_add(row.amount_stroops);
            let line = format!(
                "{} | {} | {} | {} stroops (period balance {}) | tx {} | {}",
                row.occurred_at,
                row.event_type,
                row.series_id.as_deref().unwrap_or("-"),
                row.amount_stroops,
                running_balance,
                row.chain_tx_hash.as_deref().unwrap_or("-"),
                row.description
            );
            commands.push_str(&format!("({}) Tj 0 -14 Td ", pdf_text(&line.chars().take(100).collect::<String>())));
        }
    }
    commands.push_str("ET");

    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            commands.len(),
            commands
        ),
    ];
    let mut document = String::from("%PDF-1.4\n");
    let mut offsets = vec![0usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(document.len());
        document.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
    }
    let xref_offset = document.len();
    document.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for offset in offsets.iter().skip(1) {
        document.push_str(&format!("{offset:010} 00000 n \n"));
    }
    document.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
        objects.len() + 1
    ));
    document.into_bytes()
}

/// Deterministic, wallet-scoped statement and single-record trade confirmation.
pub async fn statement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StatementQuery>,
) -> Result<Response, StatusCode> {
    let wallet = authenticated_wallet(&state.db, &headers).await?;
    let records = records(&state.db, &wallet, &query).await?;
    if query.record_id.is_some() && records.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let (content_type, filename, body) = match query.format.as_deref().unwrap_or("csv") {
        "csv" => ("text/csv; charset=utf-8", "settlement-statement.csv", csv(&records)),
        "pdf" => (
            "application/pdf",
            "settlement-statement.pdf",
            pdf(&records, query.from, query.to),
        ),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
