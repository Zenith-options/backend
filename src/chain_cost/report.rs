//! Unit-economics reporting: cost per settlement batch, cost per day,
//! distribution by operation type, grouping by account/series/action, and
//! budget utilisation. Exposed as admin JSON routes plus a Prometheus
//! scrape endpoint.
//!
//! Per-day and per-operation figures read the pre-computed
//! `chain_cost_daily_rollup`. Per-entity figures read the indexed event log.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::budget::{BudgetStatus, DAY_MS};
use super::ledger::CostLedger;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationCost {
    pub operation: String,
    pub tx_count: i64,
    pub net_cost: i64,
    pub rent_fee: i64,
    pub refund: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityCost {
    pub action: String,
    pub action_id: String,
    pub tx_count: i64,
    pub failed_tx_count: i64,
    pub net_cost: i64,
    pub unreconciled_count: i64,
    pub by_operation: Vec<OperationCost>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DailyCost {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub tx_count: i64,
    pub net_cost: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Action,
    Account,
    Series,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupCost {
    pub key: Option<String>,
    pub tx_count: i64,
    pub net_cost: i64,
}

/// Civil date for a day index (Howard Hinnant's `civil_from_days`).
pub fn day_label(day_index: i64) -> String {
    let z = day_index + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Total and per-operation cost of one business entity, e.g. a settlement batch.
pub async fn entity_cost(
    pool: &SqlitePool,
    action: &str,
    action_id: &str,
) -> Result<EntityCost, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT operation, COUNT(*), SUM(net_cost), SUM(rent_fee), SUM(refund), \
                SUM(status = 'failed_on_chain'), SUM(reconciled = 0) \
         FROM chain_cost_events WHERE action = ? AND action_id = ? \
         GROUP BY operation ORDER BY operation",
    )
    .bind(action)
    .bind(action_id)
    .fetch_all(pool)
    .await?;
    let mut out = EntityCost {
        action: action.to_string(),
        action_id: action_id.to_string(),
        tx_count: 0,
        failed_tx_count: 0,
        net_cost: 0,
        unreconciled_count: 0,
        by_operation: Vec::new(),
    };
    for r in rows {
        let op = OperationCost {
            operation: r.get(0),
            tx_count: r.get(1),
            net_cost: r.get(2),
            rent_fee: r.get(3),
            refund: r.get(4),
        };
        out.tx_count += op.tx_count;
        out.net_cost += op.net_cost;
        out.failed_tx_count += r.get::<i64, _>(5);
        out.unreconciled_count += r.get::<i64, _>(6);
        out.by_operation.push(op);
    }
    Ok(out)
}

/// Cost per UTC day over `[from_ms, to_ms)`, from the rollup.
pub async fn daily_costs(
    pool: &SqlitePool,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<DailyCost>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT day_index, SUM(tx_count), SUM(net_cost) FROM chain_cost_daily_rollup \
         WHERE day_index >= ? AND day_index < ? GROUP BY day_index ORDER BY day_index",
    )
    .bind(from_ms.div_euclid(DAY_MS))
    .bind(to_ms.div_euclid(DAY_MS) + i64::from(to_ms.rem_euclid(DAY_MS) != 0))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DailyCost {
            day: day_label(r.get(0)),
            tx_count: r.get(1),
            net_cost: r.get(2),
        })
        .collect())
}

/// Cost distribution by operation type over whole UTC days in `[from_ms, to_ms)`.
pub async fn by_operation(
    pool: &SqlitePool,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<OperationCost>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT operation, SUM(tx_count), SUM(net_cost), SUM(rent_fee), SUM(refund) FROM chain_cost_daily_rollup \
         WHERE day_index >= ? AND day_index < ? GROUP BY operation ORDER BY SUM(net_cost) DESC, operation",
    )
    .bind(from_ms.div_euclid(DAY_MS))
    .bind(to_ms.div_euclid(DAY_MS) + i64::from(to_ms.rem_euclid(DAY_MS) != 0))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| OperationCost {
            operation: r.get(0),
            tx_count: r.get(1),
            net_cost: r.get(2),
            rent_fee: r.get(3),
            refund: r.get(4),
        })
        .collect())
}

/// Group events in `[from_ms, to_ms)` by action, account or series.
pub async fn grouped(
    pool: &SqlitePool,
    dimension: Dimension,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<GroupCost>, sqlx::Error> {
    // The column comes from a closed enum, never from user input.
    let column = match dimension {
        Dimension::Action => "action",
        Dimension::Account => "account",
        Dimension::Series => "series",
    };
    let sql = format!(
        "SELECT {column}, COUNT(*), SUM(net_cost) FROM chain_cost_events \
         WHERE recorded_at_ms >= ? AND recorded_at_ms < ? GROUP BY {column} ORDER BY SUM(net_cost) DESC"
    );
    let rows = sqlx::query(&sql)
        .bind(from_ms)
        .bind(to_ms)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| GroupCost {
            key: r.get(0),
            tx_count: r.get(1),
            net_cost: r.get(2),
        })
        .collect())
}

// ─── HTTP ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    pub from_ms: i64,
    pub to_ms: i64,
    pub dimension: Option<Dimension>,
}

#[derive(Serialize)]
struct BudgetsResponse {
    budgets: Vec<BudgetStatus>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn internal(e: sqlx::Error) -> Response {
    tracing::error!(error = %e, "chain cost report query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "report unavailable" })),
    )
        .into_response()
}

/// `Some(400 response)` when the range is empty or longer than a year.
fn range_error(q: &RangeQuery) -> Option<Response> {
    (q.to_ms <= q.from_ms || q.to_ms - q.from_ms > 366 * DAY_MS).then(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "range must be non-empty and at most 366 days" })),
        )
            .into_response()
    })
}

async fn get_settlement(
    State(l): State<Arc<CostLedger>>,
    Path(batch_id): Path<String>,
) -> Response {
    match entity_cost(l.pool(), "settlement_batch", &batch_id).await {
        Ok(c) if c.tx_count == 0 => StatusCode::NOT_FOUND.into_response(),
        Ok(c) => Json(c).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_daily(State(l): State<Arc<CostLedger>>, Query(q): Query<RangeQuery>) -> Response {
    if let Some(r) = range_error(&q) {
        return r;
    }
    match daily_costs(l.pool(), q.from_ms, q.to_ms).await {
        Ok(d) => Json(d).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_operations(State(l): State<Arc<CostLedger>>, Query(q): Query<RangeQuery>) -> Response {
    if let Some(r) = range_error(&q) {
        return r;
    }
    match by_operation(l.pool(), q.from_ms, q.to_ms).await {
        Ok(d) => Json(d).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_grouped(State(l): State<Arc<CostLedger>>, Query(q): Query<RangeQuery>) -> Response {
    if let Some(r) = range_error(&q) {
        return r;
    }
    match grouped(
        l.pool(),
        q.dimension.unwrap_or(Dimension::Action),
        q.from_ms,
        q.to_ms,
    )
    .await
    {
        Ok(d) => Json(d).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_budgets(State(l): State<Arc<CostLedger>>) -> Response {
    match l.statuses(now_ms()).await {
        Ok(budgets) => Json(BudgetsResponse { budgets }).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_metrics(State(l): State<Arc<CostLedger>>) -> Response {
    match l.render_metrics(now_ms()).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], text).into_response(),
        Err(e) => internal(e),
    }
}

/// Admin reporting routes. Mount behind the admin RBAC layer:
/// `app.nest("/admin/chain-costs", chain_cost::report::router(ledger))`.
pub fn router(ledger: Arc<CostLedger>) -> Router {
    Router::new()
        .route("/settlements/:batch_id", get(get_settlement))
        .route("/daily", get(get_daily))
        .route("/operations", get(get_operations))
        .route("/grouped", get(get_grouped))
        .route("/budgets", get(get_budgets))
        .route("/metrics", get(get_metrics))
        .with_state(ledger)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_labels() {
        assert_eq!(day_label(0), "1970-01-01");
        assert_eq!(day_label(20_747), "2026-10-21");
        assert_eq!(day_label(11_016), "2000-02-29");
        assert_eq!(day_label(-1), "1969-12-31");
    }
}
