use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;

use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

const STUCK_AFTER_SECONDS: i64 = 300;
const KEEPER_STALE_AFTER_SECONDS: i64 = 600;

#[derive(Debug, Clone, Serialize)]
pub struct SettlementHealth {
    pub pending_batches: i64,
    pub stuck_transactions: i64,
    pub margin_breaches: i64,
    pub vault_coverage: Option<f64>,
    pub vault_coverage_source: String,
    pub vault_snapshot_age_seconds: Option<i64>,
    pub keeper_live: bool,
    pub keeper_last_success_age_seconds: Option<i64>,
}

pub async fn settlement_dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let expected_token = std::env::var("ZENITH_OPERATOR_TOKEN").ok();
    match authorize_operator(&headers, expected_token.as_deref()) {
        Ok(()) => {}
        Err(status) => return status.into_response(),
    }

    match load_settlement_health(&state).await {
        Ok(health) => Html(render_dashboard(&health)).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "failed to load operator settlement dashboard");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn authorize_operator(
    headers: &HeaderMap,
    expected: Option<&str>,
) -> Result<(), StatusCode> {
    let expected = expected.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    if expected.is_empty() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let mut supplied_mac =
        HmacSha256::new_from_slice(expected.as_bytes()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    supplied_mac.update(supplied.as_bytes());
    let supplied_tag = supplied_mac.finalize().into_bytes();

    let mut expected_mac =
        HmacSha256::new_from_slice(expected.as_bytes()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    expected_mac.update(expected.as_bytes());
    expected_mac
        .verify_slice(&supplied_tag)
        .map_err(|_| StatusCode::UNAUTHORIZED)
}

pub async fn load_settlement_health(
    state: &AppState,
) -> Result<SettlementHealth, sqlx::Error> {
    let pending_batches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM keeper_job_executions WHERE status IN ('pending', 'executing')",
    )
    .fetch_one(&state.db)
    .await?;

    let stuck_transactions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chain_txs
         WHERE status = 'PENDING'
           AND submitted_at <= datetime('now', ?)",
    )
    .bind(format!("-{STUCK_AFTER_SECONDS} seconds"))
    .fetch_one(&state.db)
    .await?;

    let margin_breaches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM accounts
         WHERE balance < collateral_locked",
    )
    .fetch_one(&state.db)
    .await?;

    let vault_snapshot: Option<(f64, f64, i64)> = sqlx::query_as(
        "SELECT collateral_available, liabilities,
                CAST(strftime('%s', 'now') AS INTEGER) - updated_at
         FROM vault_reserve_snapshots
         ORDER BY updated_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await?;

    let (paper_equity, paper_collateral): (f64, f64) = sqlx::query_as(
        "SELECT TOTAL(balance), TOTAL(collateral_locked) FROM accounts",
    )
    .fetch_one(&state.db)
    .await?;

    let keeper_live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM leases
         WHERE expires_at > CAST(strftime('%s', 'now') AS INTEGER)",
    )
    .fetch_one(&state.db)
    .await?;

    let keeper_last_success_age_seconds: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(strftime('%s', 'now') AS INTEGER) - MAX(executed_at)
         FROM keeper_job_executions
         WHERE status = 'completed' AND executed_at IS NOT NULL",
    )
    .fetch_one(&state.db)
    .await?;

    let (vault_coverage, vault_coverage_source, vault_snapshot_age_seconds) =
        select_vault_coverage(vault_snapshot, paper_equity, paper_collateral);

    Ok(SettlementHealth {
        pending_batches,
        stuck_transactions,
        margin_breaches,
        vault_coverage,
        vault_coverage_source,
        vault_snapshot_age_seconds,
        keeper_live: keeper_live > 0
            && keeper_last_success_age_seconds
                .is_some_and(|age| age <= KEEPER_STALE_AFTER_SECONDS),
        keeper_last_success_age_seconds,
    })
}

fn select_vault_coverage(
    vault_snapshot: Option<(f64, f64, i64)>,
    paper_equity: f64,
    paper_collateral: f64,
) -> (Option<f64>, String, Option<i64>) {
    match vault_snapshot {
        Some((collateral, liabilities, age)) if liabilities > 0.0 => (
            Some(collateral / liabilities),
            "on-chain reserve snapshot".into(),
            Some(age),
        ),
        Some((_, _, age)) => (None, "on-chain snapshot has no liabilities".into(), Some(age)),
        None if paper_collateral > 0.0 => (
            Some((paper_equity / paper_collateral).max(0.0)),
            "paper ledger estimate".into(),
            None,
        ),
        None => (None, "no reserve data".into(), None),
    }
}

fn render_dashboard(health: &SettlementHealth) -> String {
    let coverage = health
        .vault_coverage
        .map(|ratio| format!("{:.2}x", ratio))
        .unwrap_or_else(|| "unavailable".into());
    let coverage_age = health
        .vault_snapshot_age_seconds
        .map(|age| format!("{}; {age}s old", health.vault_coverage_source))
        .unwrap_or_else(|| health.vault_coverage_source.clone());
    let keeper_age = health
        .keeper_last_success_age_seconds
        .map(|age| format!("{age}s ago"))
        .unwrap_or_else(|| "no successful run recorded".into());

    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta http-equiv=\"refresh\" content=\"15\"><title>Settlement health</title>\
         <style>body{{font:16px system-ui,sans-serif;max-width:60rem;margin:3rem auto;padding:0 1rem}}\
         table{{border-collapse:collapse;width:100%}}th,td{{padding:.75rem;text-align:left;border-bottom:1px solid #ddd}}\
         .ok{{color:#16703a}}.warn{{color:#9a4c00}}</style></head><body>\
         <h1>Settlement health</h1><p>Automatically refreshed every 15 seconds.</p>\
         <table><thead><tr><th>Signal</th><th>Current state</th><th>Details</th></tr></thead><tbody>\
         <tr><th>Pending batches</th><td>{}</td><td>Keeper jobs awaiting or in execution</td></tr>\
         <tr><th>Stuck transactions</th><td>{}</td><td>Pending for at least {} seconds</td></tr>\
         <tr><th>Margin breaches</th><td>{}</td><td>Accounts below locked collateral</td></tr>\
         <tr><th>Vault coverage</th><td>{}</td><td>{}</td></tr>\
         <tr><th>Keeper liveness</th><td class=\"{}\">{}</td><td>Last successful settlement job: {}</td></tr>\
         </tbody></table></body></html>",
        health.pending_batches,
        health.stuck_transactions,
        STUCK_AFTER_SECONDS,
        health.margin_breaches,
        coverage,
        coverage_age,
        if health.keeper_live { "ok" } else { "warn" },
        if health.keeper_live { "live" } else { "stale" },
        keeper_age,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_renders_unavailable_vault_without_claiming_coverage() {
        let html = render_dashboard(&SettlementHealth {
            pending_batches: 2,
            stuck_transactions: 1,
            margin_breaches: 3,
            vault_coverage: None,
            vault_coverage_source: "no reserve data".into(),
            vault_snapshot_age_seconds: None,
            keeper_live: false,
            keeper_last_success_age_seconds: None,
        });
        assert!(html.contains("2</td><td>Keeper jobs"));
        assert!(html.contains("unavailable"));
        assert!(html.contains("no reserve data"));
        assert!(html.contains("stale"));
    }

    #[test]
    fn operator_auth_rejects_missing_or_wrong_bearer_header() {
        let headers = HeaderMap::new();
        assert_eq!(
            authorize_operator(&headers, None),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        let wrong = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        )]);
        assert_eq!(
            authorize_operator(&wrong, Some("expected")),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn operator_auth_accepts_configured_bearer_token() {
        let headers = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer expected"),
        )]);
        assert_eq!(authorize_operator(&headers, Some("expected")), Ok(()));
    }

    #[test]
    fn paper_ledger_coverage_is_labelled_as_an_estimate() {
        let (coverage, source, age) = select_vault_coverage(None, 250.0, 100.0);
        assert_eq!(coverage, Some(2.5));
        assert_eq!(source, "paper ledger estimate");
        assert_eq!(age, None);
    }
}
