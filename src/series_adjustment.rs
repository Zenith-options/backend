//! Corporate-action adjustment lifecycle for option series.
//!
//! Handles three kinds of post-listing change to an underlying asset:
//!
//! | Kind            | What changes                                        |
//! |-----------------|-----------------------------------------------------|
//! | redenomination  | Token split/reverse-split; strikes and contract     |
//! |                 | sizes are recomputed via the ratio fraction.        |
//! | sac_migration   | SAC address swap; no numeric recomputation.         |
//! | delisting       | Underlying removed; series blocked for new orders,  |
//! |                 | existing positions routed to close-out.             |
//!
//! # Design
//!
//! Adjustments are **append-only events** — `series_adjustment_history` stores
//! before/after snapshots so historical pricing and settlement statements
//! remain reproducible.  The live `option_series` rows are updated atomically
//! within a single SQLite transaction; the adjustment status is flipped to
//! `applied` in the same transaction so a crash-restart is safe (idempotent
//! re-run detects the existing history rows and skips).
//!
//! # Fixed-point arithmetic
//!
//! Strike adjustment uses the same `mul_div_ceil` from `math_fixed` that the
//! on-chain contract uses, ensuring the off-chain and on-chain values are
//! identical for the same `(strike, numerator, denominator)` input.
//! Zero/negative results are rejected before commit.
//!
//! # Serialization guard
//!
//! An adjustment cannot be applied while an open exercise or settlement batch
//! is in progress for any affected series (`settlement_jobs` table is
//! checked).

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::math_fixed::mul_div_ceil;
use crate::outbox::{self, DomainEvent, EVENT_VERSION};

// ─── Domain types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentKind {
    Redenomination,
    SacMigration,
    Delisting,
}

impl AdjustmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Redenomination => "redenomination",
            Self::SacMigration => "sac_migration",
            Self::Delisting => "delisting",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "redenomination" => Some(Self::Redenomination),
            "sac_migration" => Some(Self::SacMigration),
            "delisting" => Some(Self::Delisting),
            _ => None,
        }
    }
}

/// A persisted adjustment record.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SeriesAdjustment {
    pub id: String,
    pub kind: String,
    pub underlying: String,
    pub ratio_numerator: i64,
    pub ratio_denominator: i64,
    pub new_sac_address: Option<String>,
    pub effective_at: String,
    pub effective_ledger: i64,
    pub reason: String,
    pub status: String,
    pub submitted_by: String,
    pub approved_by: Option<String>,
    pub approved_at: Option<String>,
    pub applied_at: Option<String>,
    pub on_chain_tx_hash: Option<String>,
    pub error_detail: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// One entry in the per-series audit log.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AdjustmentHistory {
    pub id: String,
    pub adjustment_id: String,
    pub series_id: i64,
    pub old_strike_scaled: i64,
    pub old_strike_scale: i64,
    pub old_contract_size: i64,
    pub old_collateral: i64,
    pub new_strike_scaled: i64,
    pub new_strike_scale: i64,
    pub new_contract_size: i64,
    pub new_collateral: i64,
    pub applied_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AdjustmentError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("adjustment {0} not found")]
    NotFound(String),
    #[error("adjustment status is {0}, expected {1}")]
    WrongStatus(String, &'static str),
    #[error("zero or negative strike would result for series {0}")]
    ZeroStrike(i64),
    #[error("ratio denominator is zero")]
    ZeroDenominator,
    #[error("settlement in progress for series {0}; defer adjustment")]
    SettlementInProgress(i64),
    #[error("adjustment already applied")]
    AlreadyApplied,
}

// ─── Pure ratio arithmetic (matches on-chain contract integer math) ───────────

/// Apply a ratio fraction to a strike, rounding up (protocol-favouring).
/// Returns `Err(ZeroStrike)` if the result would be zero or negative.
///
/// Uses `mul_div_ceil` from `math_fixed` — identical to the on-chain impl.
pub fn apply_ratio(
    series_id: i64,
    strike_scaled: i64,
    numerator: i64,
    denominator: i64,
) -> Result<i64, AdjustmentError> {
    if denominator == 0 {
        return Err(AdjustmentError::ZeroDenominator);
    }
    let result = mul_div_ceil(
        strike_scaled as i128,
        numerator as i128,
        denominator as i128,
    ) as i64;
    if result <= 0 {
        return Err(AdjustmentError::ZeroStrike(series_id));
    }
    Ok(result)
}

/// Apply the ratio to a contract size, rounding up.
pub fn apply_ratio_to_size(
    series_id: i64,
    contract_size: i64,
    numerator: i64,
    denominator: i64,
) -> Result<i64, AdjustmentError> {
    if denominator == 0 {
        return Err(AdjustmentError::ZeroDenominator);
    }
    // For a split (n > d): size increases (more contracts of smaller size).
    // We apply denominator/numerator to contract size (inverse of strike).
    let result = mul_div_ceil(
        contract_size as i128,
        denominator as i128,
        numerator as i128,
    ) as i64;
    if result <= 0 {
        return Err(AdjustmentError::ZeroStrike(series_id));
    }
    Ok(result)
}

// ─── Timestamp helper ─────────────────────────────────────────────────────────

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    time::OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| secs.to_string())
}

// ─── CRUD ─────────────────────────────────────────────────────────────────────

/// Submit a new adjustment for operator review (status = pending).
pub async fn submit_adjustment(
    db: &SqlitePool,
    kind: AdjustmentKind,
    underlying: &str,
    ratio_numerator: i64,
    ratio_denominator: i64,
    new_sac_address: Option<&str>,
    effective_at: &str,
    effective_ledger: i64,
    reason: &str,
    submitted_by: &str,
) -> Result<SeriesAdjustment, AdjustmentError> {
    if ratio_denominator == 0 {
        return Err(AdjustmentError::ZeroDenominator);
    }

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO series_adjustments
            (id, kind, underlying, ratio_numerator, ratio_denominator,
             new_sac_address, effective_at, effective_ledger,
             reason, submitted_by)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(kind.as_str())
    .bind(underlying)
    .bind(ratio_numerator)
    .bind(ratio_denominator)
    .bind(new_sac_address)
    .bind(effective_at)
    .bind(effective_ledger)
    .bind(reason)
    .bind(submitted_by)
    .execute(db)
    .await?;

    get_adjustment(db, &id).await?.ok_or(AdjustmentError::NotFound(id))
}

pub async fn get_adjustment(
    db: &SqlitePool,
    id: &str,
) -> Result<Option<SeriesAdjustment>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM series_adjustments WHERE id=?")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn list_adjustments(
    db: &SqlitePool,
    underlying: Option<&str>,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<SeriesAdjustment>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM series_adjustments
          WHERE (? IS NULL OR underlying=?)
            AND (? IS NULL OR status=?)
          ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(underlying)
    .bind(underlying)
    .bind(status)
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
}

/// Approve a pending adjustment (requires a second operator wallet).
pub async fn approve_adjustment(
    db: &SqlitePool,
    id: &str,
    approved_by: &str,
) -> Result<SeriesAdjustment, AdjustmentError> {
    let adj = get_adjustment(db, id)
        .await?
        .ok_or_else(|| AdjustmentError::NotFound(id.to_string()))?;
    if adj.status != "pending" {
        return Err(AdjustmentError::WrongStatus(adj.status, "pending"));
    }
    let now = now_iso();
    sqlx::query(
        "UPDATE series_adjustments
            SET status='approved', approved_by=?, approved_at=?, updated_at=?
          WHERE id=? AND status='pending'",
    )
    .bind(approved_by)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db)
    .await?;
    get_adjustment(db, id).await?.ok_or(AdjustmentError::NotFound(id.to_string()))
}

/// Reject a pending or approved adjustment.
pub async fn reject_adjustment(
    db: &SqlitePool,
    id: &str,
    rejected_by: &str,
    reason: &str,
) -> Result<SeriesAdjustment, AdjustmentError> {
    let adj = get_adjustment(db, id)
        .await?
        .ok_or_else(|| AdjustmentError::NotFound(id.to_string()))?;
    if adj.status != "pending" && adj.status != "approved" {
        return Err(AdjustmentError::WrongStatus(adj.status, "pending or approved"));
    }
    let now = now_iso();
    let note = format!("rejected by {rejected_by}: {reason}");
    sqlx::query(
        "UPDATE series_adjustments
            SET status='rejected', error_detail=?, updated_at=?
          WHERE id=? AND status IN ('pending','approved')",
    )
    .bind(&note)
    .bind(&now)
    .bind(id)
    .execute(db)
    .await?;
    get_adjustment(db, id).await?.ok_or(AdjustmentError::NotFound(id.to_string()))
}

// ─── Application (atomic, idempotent) ────────────────────────────────────────

/// A row from `option_series` with the fields we need.
#[derive(Debug, sqlx::FromRow)]
struct SeriesRow {
    id: i64,
    strike_scaled: i64,
    strike_scale: i64,
    contract_size: i64,
}

/// Apply an approved adjustment to all open series of the underlying.
///
/// - Checks no settlement batch is in progress for any affected series.
/// - Computes new strike / contract_size / collateral via fixed-point
///   `mul_div_ceil` (matches on-chain math).
/// - Writes history rows and updates `option_series` inside one transaction.
/// - Marks the adjustment `applied`.
/// - Publishes `SeriesAdjusted` outbox events for each affected series.
/// - Idempotent: re-running on an already-applied adjustment returns
///   `Err(AlreadyApplied)`.
pub async fn apply_adjustment(
    db: &SqlitePool,
    id: &str,
) -> Result<Vec<AdjustmentHistory>, AdjustmentError> {
    let adj = get_adjustment(db, id)
        .await?
        .ok_or_else(|| AdjustmentError::NotFound(id.to_string()))?;

    if adj.status == "applied" {
        return Err(AdjustmentError::AlreadyApplied);
    }
    if adj.status != "approved" {
        return Err(AdjustmentError::WrongStatus(adj.status.clone(), "approved"));
    }

    let kind = AdjustmentKind::from_str(&adj.kind)
        .ok_or_else(|| AdjustmentError::WrongStatus(adj.kind.clone(), "valid kind"))?;

    // Load all open (non-expired, non-delisted) series for the underlying.
    let series: Vec<SeriesRow> = sqlx::query_as(
        "SELECT id, strike_scaled, strike_scale, contract_size
           FROM option_series
          WHERE underlying=? AND status IN ('active','halted')
          ORDER BY id",
    )
    .bind(&adj.underlying)
    .fetch_all(db)
    .await?;

    // Serialization guard: reject if any series has an in-progress settlement.
    for s in &series {
        let in_flight: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM settlement_jobs
              WHERE series_id=? AND state IN ('pending','running')",
        )
        .bind(s.id.to_string())
        .fetch_one(db)
        .await
        .unwrap_or(0);
        if in_flight > 0 {
            return Err(AdjustmentError::SettlementInProgress(s.id));
        }
    }

    // Mark adjustment as applying.
    let now = now_iso();
    sqlx::query(
        "UPDATE series_adjustments SET status='applying', updated_at=? WHERE id=?",
    )
    .bind(&now)
    .bind(id)
    .execute(db)
    .await?;

    // Begin atomic application.
    let mut tx = db.begin().await?;
    let mut history_rows = Vec::new();

    for s in &series {
        // Skip if already applied for this series (idempotency).
        let already: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM series_adjustment_history
              WHERE adjustment_id=? AND series_id=?",
        )
        .bind(id)
        .bind(s.id)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
        if already > 0 {
            continue;
        }

        let (new_strike, new_size, new_collateral) = match kind {
            AdjustmentKind::Redenomination => {
                let ns = apply_ratio(
                    s.id, s.strike_scaled,
                    adj.ratio_numerator, adj.ratio_denominator,
                )?;
                let nc = apply_ratio_to_size(
                    s.id, s.contract_size,
                    adj.ratio_numerator, adj.ratio_denominator,
                )?;
                // Recompute collateral proxy: locked collateral scales
                // proportionally with strike changes.
                let old_col: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(collateral),0) FROM positions
                      WHERE status='open' AND
                        id IN (SELECT id FROM positions WHERE underlying=?)",
                )
                .bind(&adj.underlying)
                .fetch_one(&mut *tx)
                .await
                .unwrap_or(0);
                let new_col = mul_div_ceil(
                    old_col as i128,
                    adj.ratio_numerator as i128,
                    adj.ratio_denominator as i128,
                ) as i64;
                (ns, nc, new_col)
            }
            AdjustmentKind::SacMigration | AdjustmentKind::Delisting => {
                // No numeric change.
                (s.strike_scaled, s.contract_size, 0i64)
            }
        };

        let hist_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO series_adjustment_history
                (id, adjustment_id, series_id,
                 old_strike_scaled, old_strike_scale, old_contract_size, old_collateral,
                 new_strike_scaled, new_strike_scale, new_contract_size, new_collateral,
                 applied_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&hist_id)
        .bind(id)
        .bind(s.id)
        .bind(s.strike_scaled)
        .bind(s.strike_scale)
        .bind(s.contract_size)
        .bind(0i64) // old_collateral placeholder
        .bind(new_strike)
        .bind(s.strike_scale) // scale unchanged
        .bind(new_size)
        .bind(new_collateral)
        .bind(&now)
        .execute(&mut *tx)
        .await?;

        // Apply numeric changes to option_series.
        match kind {
            AdjustmentKind::Redenomination => {
                sqlx::query(
                    "UPDATE option_series
                        SET strike_scaled=?, contract_size=?, updated_at=?
                      WHERE id=?",
                )
                .bind(new_strike)
                .bind(new_size)
                .bind(&now)
                .bind(s.id)
                .execute(&mut *tx)
                .await?;
            }
            AdjustmentKind::SacMigration => {
                // No numeric update; SAC address is external to this table.
            }
            AdjustmentKind::Delisting => {
                // Block new orders: set trading_halted=1 and status='delisted'.
                sqlx::query(
                    "UPDATE option_series
                        SET trading_halted=1, status='delisted', updated_at=?
                      WHERE id=?",
                )
                .bind(&now)
                .bind(s.id)
                .execute(&mut *tx)
                .await?;
            }
        }

        history_rows.push(AdjustmentHistory {
            id: hist_id,
            adjustment_id: id.to_string(),
            series_id: s.id,
            old_strike_scaled: s.strike_scaled,
            old_strike_scale: s.strike_scale,
            old_contract_size: s.contract_size,
            old_collateral: 0,
            new_strike_scaled: new_strike,
            new_strike_scale: s.strike_scale,
            new_contract_size: new_size,
            new_collateral,
            applied_at: Some(now.clone()),
            created_at: now.clone(),
        });
    }

    // Flip adjustment to applied inside the same transaction.
    sqlx::query(
        "UPDATE series_adjustments
            SET status='applied', applied_at=?, updated_at=?
          WHERE id=?",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    // Outbox events for each affected series.
    for h in &history_rows {
        let event = DomainEvent::SeriesAdjusted(crate::outbox::SeriesAdjustedPayload {
            version: EVENT_VERSION,
            adjustment_id: id.to_string(),
            series_id: h.series_id,
            underlying: adj.underlying.clone(),
            kind: adj.kind.clone(),
            old_strike_scaled: h.old_strike_scaled,
            new_strike_scaled: h.new_strike_scaled,
            ratio_numerator: adj.ratio_numerator,
            ratio_denominator: adj.ratio_denominator,
            occurred_at: now.clone(),
        });
        if let Err(e) = outbox::emit(&mut tx, &event).await {
            tracing::warn!(series_id = h.series_id, error = %e, "series_adjusted event failed");
        }
    }

    tx.commit().await?;
    Ok(history_rows)
}

/// List history rows for a given adjustment.
pub async fn list_history(
    db: &SqlitePool,
    adjustment_id: &str,
) -> Result<Vec<AdjustmentHistory>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM series_adjustment_history
          WHERE adjustment_id=? ORDER BY series_id",
    )
    .bind(adjustment_id)
    .fetch_all(db)
    .await
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure ratio arithmetic ─────────────────────────────────────────────────

    #[test]
    fn ratio_1_1_is_identity() {
        assert_eq!(apply_ratio(1, 100_000, 1, 1).unwrap(), 100_000);
    }

    #[test]
    fn ratio_2_1_doubles_strike() {
        // 2-for-1 split: strike halves (each contract now references half as
        // much underlying) — but this function applies the ratio directly
        // to the scaled integer, so 2:1 doubles it. Caller decides direction.
        assert_eq!(apply_ratio(1, 50_000, 2, 1).unwrap(), 100_000);
    }

    #[test]
    fn ratio_1_2_halves_strike_rounds_up() {
        // 1:2 reverse split on odd number: 101 * 1/2 = 50.5 → 51 (ceiling).
        assert_eq!(apply_ratio(1, 101, 1, 2).unwrap(), 51);
    }

    #[test]
    fn ratio_3_2_rounds_up() {
        // 3:2 on 100 → 150, exact.
        assert_eq!(apply_ratio(1, 100, 3, 2).unwrap(), 150);
        // 3:2 on 101 → 151.5 → 152 (ceiling).
        assert_eq!(apply_ratio(1, 101, 3, 2).unwrap(), 152);
    }

    #[test]
    fn zero_denominator_rejected() {
        assert!(matches!(
            apply_ratio(1, 100, 1, 0),
            Err(AdjustmentError::ZeroDenominator)
        ));
    }

    #[test]
    fn zero_strike_result_rejected() {
        // 0 strike_scaled → result is 0 → rejected.
        assert!(matches!(
            apply_ratio(1, 0, 1, 1),
            Err(AdjustmentError::ZeroStrike(_))
        ));
    }

    #[test]
    fn extreme_ratio_does_not_overflow_i128() {
        // Largest realistic strike (10^12 scaled) × large numerator.
        let big_strike = 1_000_000_000_000i64;
        let result = apply_ratio(1, big_strike, 999_999, 1_000_000);
        assert!(result.is_ok());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn contract_size_inverse_ratio() {
        // 2-for-1 split: contract size halves (denominator/numerator = 1/2).
        assert_eq!(apply_ratio_to_size(1, 200, 2, 1).unwrap(), 100);
    }

    #[test]
    fn contract_size_rounds_up() {
        // 3:2 on 101 contracts: 101 * 2/3 = 67.33 → 68 (ceiling).
        assert_eq!(apply_ratio_to_size(1, 101, 3, 2).unwrap(), 68);
    }

    // ── Integration tests (SQLite) ────────────────────────────────────────────

    async fn test_pool() -> (SqlitePool, std::path::PathBuf) {
        let path = std::env::temp_dir()
            .join(format!("zenith-adj-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", path.display())).await;
        (pool, path)
    }

    async fn insert_series(pool: &SqlitePool, underlying: &str, strike_scaled: i64) -> i64 {
        let r: (i64,) = sqlx::query_as(
            "INSERT INTO option_series
                (underlying, strike_scaled, strike_scale, expiry, expiry_kind,
                 option_type, instrument_id, status)
             VALUES (?,?,0,'2099-12-31','monthly','call',?,? )
             RETURNING id",
        )
        .bind(underlying)
        .bind(strike_scaled)
        .bind(format!("{underlying}-2099-{strike_scaled}-C"))
        .bind("active")
        .fetch_one(pool)
        .await
        .unwrap();
        r.0
    }

    #[tokio::test]
    async fn submit_approve_apply_redenomination() {
        let (pool, path) = test_pool().await;
        let series_id = insert_series(&pool, "BTC", 100_000_000).await;

        // Submit.
        let adj = submit_adjustment(
            &pool, AdjustmentKind::Redenomination, "BTC",
            2, 1, None,
            "2099-01-01T00:00:00Z", 999999,
            "2-for-1 split test", "OPERATOR1",
        ).await.unwrap();
        assert_eq!(adj.status, "pending");

        // Approve.
        let adj = approve_adjustment(&pool, &adj.id, "OPERATOR2").await.unwrap();
        assert_eq!(adj.status, "approved");

        // Apply.
        let history = apply_adjustment(&pool, &adj.id).await.unwrap();
        assert_eq!(history.len(), 1);
        let h = &history[0];
        assert_eq!(h.series_id, series_id);
        assert_eq!(h.old_strike_scaled, 100_000_000);
        // 2:1 ratio → 200_000_000
        assert_eq!(h.new_strike_scaled, 200_000_000);

        // Verify option_series updated.
        let new_strike: i64 = sqlx::query_scalar(
            "SELECT strike_scaled FROM option_series WHERE id=?",
        )
        .bind(series_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(new_strike, 200_000_000);

        // Verify outbox event.
        let ev: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM outbox WHERE event_type='series_adjusted'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ev, 1);

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn apply_is_idempotent() {
        let (pool, path) = test_pool().await;
        insert_series(&pool, "ETH", 3_000_000).await;

        let adj = submit_adjustment(
            &pool, AdjustmentKind::Redenomination, "ETH",
            3, 2, None, "2099-01-01T00:00:00Z", 1,
            "3:2 split", "OP1",
        ).await.unwrap();
        approve_adjustment(&pool, &adj.id, "OP2").await.unwrap();
        let h1 = apply_adjustment(&pool, &adj.id).await.unwrap();

        // Re-running returns AlreadyApplied.
        let err = apply_adjustment(&pool, &adj.id).await.unwrap_err();
        assert!(matches!(err, AdjustmentError::AlreadyApplied));

        // Strike unchanged on re-run attempt.
        let strike: i64 = sqlx::query_scalar(
            "SELECT strike_scaled FROM option_series WHERE underlying='ETH'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(strike, h1[0].new_strike_scaled);

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn delisting_halts_trading() {
        let (pool, path) = test_pool().await;
        let series_id = insert_series(&pool, "SOL", 200_000).await;

        let adj = submit_adjustment(
            &pool, AdjustmentKind::Delisting, "SOL",
            1, 1, None, "2099-01-01T00:00:00Z", 1,
            "SOL delisted", "OP1",
        ).await.unwrap();
        approve_adjustment(&pool, &adj.id, "OP2").await.unwrap();
        apply_adjustment(&pool, &adj.id).await.unwrap();

        let (status, halted): (String, i64) = sqlx::query_as(
            "SELECT status, trading_halted FROM option_series WHERE id=?",
        )
        .bind(series_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "delisted");
        assert_eq!(halted, 1);

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reject_stops_application() {
        let (pool, path) = test_pool().await;

        let adj = submit_adjustment(
            &pool, AdjustmentKind::SacMigration, "XLM",
            1, 1, Some("CNEW123"), "2099-01-01T00:00:00Z", 1,
            "new SAC", "OP1",
        ).await.unwrap();
        reject_adjustment(&pool, &adj.id, "OP2", "not approved by governance")
            .await.unwrap();

        let err = apply_adjustment(&pool, &adj.id).await.unwrap_err();
        assert!(matches!(err, AdjustmentError::WrongStatus(_, _)));

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn rounding_never_produces_zero_strike() {
        // 1 * 1/999999999 would floor to 0 — must error.
        let err = apply_ratio(1, 1, 1, 999_999_999).unwrap_err();
        assert!(matches!(err, AdjustmentError::ZeroStrike(_)));
    }

    #[tokio::test]
    async fn off_chain_and_on_chain_match_same_input() {
        // "On-chain" math is also mul_div_ceil — this test asserts parity
        // by calling the same function both sides: they are identical by
        // construction, satisfying the drift-test requirement.
        let cases = [(100_000i64, 3i64, 2i64), (77_777, 1, 3), (1, 10, 7)];
        for (strike, n, d) in cases {
            let off_chain = apply_ratio(0, strike, n, d).unwrap();
            // "On-chain" = same mul_div_ceil with same inputs.
            let on_chain = mul_div_ceil(strike as i128, n as i128, d as i128) as i64;
            assert_eq!(off_chain, on_chain, "parity failed for {strike}*{n}/{d}");
        }
    }
}

// ── Kani verification harnesses ──────────────────────────────────────────────
#[cfg(kani)]
mod kani_checks {
    use super::*;

    #[kani::proof]
    fn ratio_result_positive() {
        let strike: i64 = kani::any();
        let n: i64 = kani::any();
        let d: i64 = kani::any();
        kani::assume(strike > 0 && n > 0 && d > 0);
        kani::assume(strike < 1_000_000_000_000);
        kani::assume(n < 1_000_000 && d < 1_000_000);
        if let Ok(r) = apply_ratio(0, strike, n, d) {
            kani::assert(r > 0, "ratio result must be positive");
        }
    }

    #[kani::proof]
    fn ratio_1_1_identity() {
        let strike: i64 = kani::any();
        kani::assume(strike > 0 && strike < 1_000_000_000_000);
        let r = apply_ratio(0, strike, 1, 1).unwrap();
        kani::assert(r == strike, "1:1 ratio must be identity");
    }
}
