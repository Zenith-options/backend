use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::AppState;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct KeeperBounty {
    pub job_id: String,
    pub task_name: String,
    pub keeper_wallet: String,
    pub amount_stroops: i64,
    pub status: String,
    pub payout_tx_hash: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BountyBalance {
    pub accrued_stroops: i64,
    pub paid_stroops: i64,
    pub claimable_stroops: i64,
}

#[derive(Debug)]
pub enum BountyError {
    InvalidIdentity,
    InvalidAmount,
    Database(sqlx::Error),
}

impl std::fmt::Display for BountyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIdentity => write!(f, "keeper wallet and job id must not be empty"),
            Self::InvalidAmount => write!(f, "bounty amount must be positive"),
            Self::Database(error) => write!(f, "database error: {error}"),
        }
    }
}

impl std::error::Error for BountyError {}

impl From<sqlx::Error> for BountyError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Accrue a reward once for a completed keeper job. The job id is the
/// idempotency key, so retrying after a worker crash cannot credit twice.
pub async fn accrue(
    pool: &SqlitePool,
    job_id: &str,
    task_name: &str,
    keeper_wallet: &str,
    amount_stroops: i64,
) -> Result<bool, BountyError> {
    if job_id.trim().is_empty() || task_name.trim().is_empty() || keeper_wallet.trim().is_empty() {
        return Err(BountyError::InvalidIdentity);
    }
    if amount_stroops <= 0 {
        return Err(BountyError::InvalidAmount);
    }

    let result = sqlx::query(
        "INSERT INTO keeper_bounties (job_id, task_name, keeper_wallet, amount_stroops)
         SELECT job_id, ?2, ?3, ?4
           FROM keeper_job_executions
          WHERE job_id = ?1 AND status = 'completed'
         ON CONFLICT(job_id) DO NOTHING",
    )
    .bind(job_id)
    .bind(task_name)
    .bind(keeper_wallet)
    .bind(amount_stroops)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn balance(
    pool: &SqlitePool,
    keeper_wallet: &str,
) -> Result<BountyBalance, BountyError> {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT
             COALESCE(SUM(amount_stroops), 0),
             COALESCE(SUM(CASE WHEN status = 'paid' THEN amount_stroops ELSE 0 END), 0)
           FROM keeper_bounties
          WHERE keeper_wallet = ?1",
    )
    .bind(keeper_wallet)
    .fetch_one(pool)
    .await?;
    Ok(BountyBalance {
        accrued_stroops: row.0,
        paid_stroops: row.1,
        claimable_stroops: row.0 - row.1,
    })
}

pub async fn list(
    pool: &SqlitePool,
    keeper_wallet: &str,
) -> Result<Vec<KeeperBounty>, BountyError> {
    Ok(sqlx::query_as(
        "SELECT job_id, task_name, keeper_wallet, amount_stroops, status,
                payout_tx_hash, created_at
           FROM keeper_bounties
          WHERE keeper_wallet = ?1
          ORDER BY created_at DESC, job_id DESC",
    )
    .bind(keeper_wallet)
    .fetch_all(pool)
    .await?)
}

async fn authenticated_wallet(pool: &SqlitePool, headers: &HeaderMap) -> Result<String, StatusCode> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
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

/// Keeper-authenticated accounting surface. A keeper can only see its own
/// accruals; payouts are finalized separately after a confirmed chain transfer.
pub async fn accounting(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let wallet = authenticated_wallet(&state.db, &headers).await?;
    let accrued = balance(&state.db, &wallet)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let jobs = list(&state.db, &wallet)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "wallet": wallet,
        "balance": accrued,
        "jobs": jobs,
    })))
}
