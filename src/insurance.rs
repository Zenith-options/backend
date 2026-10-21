//! Insurance reserve and liquidation deficit ledger.
//!
//! When a liquidation can't recover an account's full shortfall, the gap is
//! recorded as a **deficit** instead of disappearing into a negative balance
//! nobody reports. Each deficit is covered from the reserve as far as the
//! reserve allows; whatever is left stays `open` until later contributions
//! cover it ([`cover_open_deficits`]) or governance writes it off
//! ([`write_off`]). Nothing is ever netted away silently.
//!
//! Invariants, checked by [`verify`]:
//!
//! * the reserve balance equals the sum of its append-only journal;
//! * every deficit's `covered` equals the cover draws journaled against it;
//! * `outstanding = shortfall - covered - written_off` is never negative.
//!
//! All amounts are `i64` stroops (1e-7) of the settlement asset.
//!
//! Concurrency: every mutating transaction starts with a write, which takes
//! SQLite's write lock before any balance is read, so two concurrent draws
//! can't both spend the same reserve.

use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use sqlx::{Sqlite, SqlitePool, Transaction};

/// Stroops per whole unit of the settlement asset.
pub const STROOPS_PER_UNIT: i64 = 10_000_000;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// Reserve inflow kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflowKind {
    Contribution,
    LiquidationPenalty,
    FeeShare,
}

impl InflowKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Contribution => "contribution",
            Self::LiquidationPenalty => "liquidation_penalty",
            Self::FeeShare => "fee_share",
        }
    }
}

#[derive(Debug)]
pub enum ReserveError {
    Db(sqlx::Error),
    /// Amounts must be strictly positive.
    InvalidAmount(i64),
    /// A withdrawal larger than the reserve.
    InsufficientReserve {
        balance: i64,
        requested: i64,
    },
    /// Withdrawals are blocked while any deficit is still open.
    OpenDeficits {
        outstanding: i64,
    },
    DeficitNotFound(String),
    /// The same liquidation was recorded again with a different shortfall.
    ConflictingShortfall {
        liquidation_ref: String,
        recorded: i64,
        requested: i64,
    },
    /// Write-off on a deficit that has nothing outstanding, or for more
    /// than is outstanding.
    InvalidWriteOff {
        outstanding: i64,
        requested: i64,
    },
}

impl std::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database error: {e}"),
            Self::InvalidAmount(a) => write!(f, "amount must be positive, got {a}"),
            Self::InsufficientReserve { balance, requested } => {
                write!(f, "reserve balance {balance} is less than {requested}")
            }
            Self::OpenDeficits { outstanding } => write!(
                f,
                "withdrawals are blocked while {outstanding} stroops of deficits are open"
            ),
            Self::DeficitNotFound(id) => write!(f, "deficit {id} not found"),
            Self::ConflictingShortfall {
                liquidation_ref,
                recorded,
                requested,
            } => write!(
                f,
                "liquidation {liquidation_ref} already recorded with shortfall {recorded}, \
                 not {requested}"
            ),
            Self::InvalidWriteOff {
                outstanding,
                requested,
            } => write!(f, "cannot write off {requested}: {outstanding} outstanding"),
        }
    }
}

impl std::error::Error for ReserveError {}

impl From<sqlx::Error> for ReserveError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// A liquidation that ended with equity below zero.
#[derive(Debug, Clone)]
pub struct Shortfall<'a> {
    /// Identifier of the liquidation; recording the same one twice is a
    /// no-op that returns the original outcome.
    pub liquidation_ref: &'a str,
    pub account: &'a str,
    pub underlying: Option<&'a str>,
    /// Unrecovered amount in stroops, > 0.
    pub amount: i64,
}

/// Result of recording a shortfall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeficitOutcome {
    pub deficit_id: String,
    pub shortfall: i64,
    /// Covered by the reserve (in total, including earlier draws).
    pub covered: i64,
    pub written_off: i64,
    /// Still unsettled. Non-zero means the reserve couldn't absorb it.
    pub outstanding: i64,
    /// True if this liquidation had been recorded before.
    pub already_recorded: bool,
}

/// The published coverage metric.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageReport {
    pub reserve_balance: i64,
    pub open_deficits: i64,
    pub outstanding_deficit: i64,
    /// `reserve_balance - outstanding_deficit`; negative means the protocol
    /// has more unsettled liability than reserve.
    pub net_reserve: i64,
    pub total_shortfall: i64,
    pub total_covered: i64,
    pub total_written_off: i64,
    /// Collateral backing open short positions — the liability base the
    /// reserve stands behind.
    pub at_risk_collateral: i64,
    /// `net_reserve / at_risk_collateral` in basis points; `None` when
    /// nothing is at risk.
    pub coverage_ratio_bps: Option<i64>,
    /// Share of all recorded shortfalls that the reserve has covered, in
    /// basis points; `None` before the first shortfall.
    pub recovery_rate_bps: Option<i64>,
}

/// How much of `outstanding` the reserve can cover right now.
pub fn cover_amount(balance: i64, outstanding: i64) -> i64 {
    balance.min(outstanding).max(0)
}

/// `numerator / denominator` in basis points, rounded toward zero, without
/// overflow. `None` when the denominator is not positive.
pub fn ratio_bps(numerator: i64, denominator: i64) -> Option<i64> {
    if denominator <= 0 {
        return None;
    }
    let bps = (numerator as i128 * 10_000) / denominator as i128;
    Some(bps.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// Converts an `f64` amount of the settlement asset to stroops, rounding
/// to nearest.
pub fn to_stroops(units: f64) -> i64 {
    (units * STROOPS_PER_UNIT as f64).round() as i64
}

/// Takes the write lock and returns the current reserve balance.
async fn lock_reserve(tx: &mut Transaction<'_, Sqlite>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "UPDATE insurance_reserve_state SET updated_at = {NOW} WHERE id = 1 RETURNING balance"
    ))
    .fetch_one(&mut **tx)
    .await
}

async fn journal(
    tx: &mut Transaction<'_, Sqlite>,
    kind: &str,
    amount: i64,
    deficit_id: Option<&str>,
    reference: Option<&str>,
    memo: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let balance: i64 = sqlx::query_scalar(&format!(
        "UPDATE insurance_reserve_state SET balance = balance + ?1, updated_at = {NOW} \
         WHERE id = 1 RETURNING balance"
    ))
    .bind(amount)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO insurance_reserve_entries \
         (kind, amount, balance_after, deficit_id, reference, memo) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(kind)
    .bind(amount)
    .bind(balance)
    .bind(deficit_id)
    .bind(reference)
    .bind(memo)
    .execute(&mut **tx)
    .await?;
    Ok(balance)
}

/// Settles part of a deficit: adds `covered`/`written_off` and moves the
/// status to `covered` / `written_off` once nothing is outstanding.
async fn settle_deficit(
    tx: &mut Transaction<'_, Sqlite>,
    deficit_id: &str,
    covered: i64,
    written_off: i64,
    memo: Option<&str>,
) -> Result<(), sqlx::Error> {
    // SQLite evaluates every SET expression against the pre-update row.
    sqlx::query(&format!(
        "UPDATE liquidation_deficits SET
            covered = covered + ?2,
            written_off = written_off + ?3,
            write_off_memo = COALESCE(?4, write_off_memo),
            status = CASE
                WHEN covered + ?2 + written_off + ?3 < shortfall THEN 'open'
                WHEN written_off + ?3 > 0 THEN 'written_off'
                ELSE 'covered' END,
            resolved_at = CASE
                WHEN covered + ?2 + written_off + ?3 < shortfall THEN NULL
                ELSE {NOW} END
         WHERE id = ?1"
    ))
    .bind(deficit_id)
    .bind(covered)
    .bind(written_off)
    .bind(memo)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn fetch_deficit(
    tx: &mut Transaction<'_, Sqlite>,
    where_sql: &str,
    key: &str,
) -> Result<Option<(String, i64, i64, i64)>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT id, shortfall, covered, written_off FROM liquidation_deficits WHERE {where_sql} = ?1"
    ))
    .bind(key)
    .fetch_optional(&mut **tx)
    .await
}

fn outcome(row: (String, i64, i64, i64), already_recorded: bool) -> DeficitOutcome {
    let (deficit_id, shortfall, covered, written_off) = row;
    DeficitOutcome {
        deficit_id,
        shortfall,
        covered,
        written_off,
        outstanding: shortfall - covered - written_off,
        already_recorded,
    }
}

/// Adds money to the reserve. Does not apply it to open deficits; call
/// [`cover_open_deficits`] for that. Returns the new balance.
pub async fn contribute(
    pool: &SqlitePool,
    kind: InflowKind,
    amount: i64,
    reference: Option<&str>,
    memo: Option<&str>,
) -> Result<i64, ReserveError> {
    if amount <= 0 {
        return Err(ReserveError::InvalidAmount(amount));
    }
    let mut tx = pool.begin().await?;
    let balance = journal(&mut tx, kind.as_str(), amount, None, reference, memo).await?;
    tx.commit().await?;
    Ok(balance)
}

/// Governance-approved withdrawal. Refused while any deficit is open: the
/// reserve exists to cover those first.
pub async fn withdraw(pool: &SqlitePool, amount: i64, memo: &str) -> Result<i64, ReserveError> {
    if amount <= 0 {
        return Err(ReserveError::InvalidAmount(amount));
    }
    let mut tx = pool.begin().await?;
    let balance = lock_reserve(&mut tx).await?;
    let outstanding: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(shortfall - covered - written_off), 0) \
         FROM liquidation_deficits WHERE status = 'open'",
    )
    .fetch_one(&mut *tx)
    .await?;
    if outstanding > 0 {
        return Err(ReserveError::OpenDeficits { outstanding });
    }
    if amount > balance {
        return Err(ReserveError::InsufficientReserve {
            balance,
            requested: amount,
        });
    }
    let balance = journal(&mut tx, "withdrawal", -amount, None, None, Some(memo)).await?;
    tx.commit().await?;
    Ok(balance)
}

/// Records a liquidation shortfall and covers as much of it as the reserve
/// can. Idempotent on `liquidation_ref`.
pub async fn record_shortfall(
    pool: &SqlitePool,
    s: &Shortfall<'_>,
) -> Result<DeficitOutcome, ReserveError> {
    if s.amount <= 0 {
        return Err(ReserveError::InvalidAmount(s.amount));
    }
    let mut tx = pool.begin().await?;
    let deficit_id = uuid::Uuid::new_v4().to_string();
    let inserted = sqlx::query(
        "INSERT INTO liquidation_deficits (id, liquidation_ref, account, underlying, shortfall) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (liquidation_ref) DO NOTHING",
    )
    .bind(&deficit_id)
    .bind(s.liquidation_ref)
    .bind(s.account)
    .bind(s.underlying)
    .bind(s.amount)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if inserted == 0 {
        let row = fetch_deficit(&mut tx, "liquidation_ref", s.liquidation_ref)
            .await?
            .ok_or_else(|| ReserveError::DeficitNotFound(s.liquidation_ref.to_string()))?;
        if row.1 != s.amount {
            return Err(ReserveError::ConflictingShortfall {
                liquidation_ref: s.liquidation_ref.to_string(),
                recorded: row.1,
                requested: s.amount,
            });
        }
        tx.commit().await?;
        return Ok(outcome(row, true));
    }

    // The INSERT above already holds the write lock.
    let balance = lock_reserve(&mut tx).await?;
    let cover = cover_amount(balance, s.amount);
    if cover > 0 {
        journal(
            &mut tx,
            "deficit_cover",
            -cover,
            Some(&deficit_id),
            Some(s.liquidation_ref),
            None,
        )
        .await?;
        settle_deficit(&mut tx, &deficit_id, cover, 0, None).await?;
    }
    if cover < s.amount {
        tracing::warn!(
            liquidation_ref = s.liquidation_ref,
            account = s.account,
            shortfall = s.amount,
            covered = cover,
            outstanding = s.amount - cover,
            "insurance reserve could not fully cover liquidation shortfall"
        );
    }
    let row = fetch_deficit(&mut tx, "id", &deficit_id)
        .await?
        .ok_or_else(|| ReserveError::DeficitNotFound(deficit_id.clone()))?;
    tx.commit().await?;
    Ok(outcome(row, false))
}

/// Applies the reserve to open deficits, oldest first, until either runs
/// out. Returns the deficits that received cover.
pub async fn cover_open_deficits(pool: &SqlitePool) -> Result<Vec<DeficitOutcome>, ReserveError> {
    let mut tx = pool.begin().await?;
    let mut balance = lock_reserve(&mut tx).await?;
    let open: Vec<(String, i64, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT id, shortfall, covered, written_off, liquidation_ref FROM liquidation_deficits \
         WHERE status = 'open' ORDER BY created_at, id",
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut applied = Vec::new();
    for (id, shortfall, covered, written_off, liq_ref) in open {
        if balance == 0 {
            break;
        }
        let cover = cover_amount(balance, shortfall - covered - written_off);
        if cover == 0 {
            continue;
        }
        balance = journal(
            &mut tx,
            "deficit_cover",
            -cover,
            Some(&id),
            liq_ref.as_deref(),
            None,
        )
        .await?;
        settle_deficit(&mut tx, &id, cover, 0, None).await?;
        applied.push(outcome((id, shortfall, covered + cover, written_off), true));
    }
    tx.commit().await?;
    Ok(applied)
}

/// Explicitly writes off part (or, with `None`, all) of a deficit's
/// outstanding amount — the governance decision to socialise a loss. The
/// write-off is recorded on the deficit with its memo; the reserve is not
/// touched.
pub async fn write_off(
    pool: &SqlitePool,
    deficit_id: &str,
    amount: Option<i64>,
    memo: &str,
) -> Result<DeficitOutcome, ReserveError> {
    let mut tx = pool.begin().await?;
    lock_reserve(&mut tx).await?;
    let row = fetch_deficit(&mut tx, "id", deficit_id)
        .await?
        .ok_or_else(|| ReserveError::DeficitNotFound(deficit_id.to_string()))?;
    let outstanding = row.1 - row.2 - row.3;
    let requested = amount.unwrap_or(outstanding);
    if requested <= 0 || requested > outstanding {
        return Err(ReserveError::InvalidWriteOff {
            outstanding,
            requested,
        });
    }
    settle_deficit(&mut tx, deficit_id, 0, requested, Some(memo)).await?;
    let row = fetch_deficit(&mut tx, "id", deficit_id)
        .await?
        .ok_or_else(|| ReserveError::DeficitNotFound(deficit_id.to_string()))?;
    tx.commit().await?;
    Ok(outcome(row, true))
}

/// Builds the coverage report. `at_risk_collateral` is in stroops.
pub async fn coverage(
    pool: &SqlitePool,
    at_risk_collateral: i64,
) -> Result<CoverageReport, ReserveError> {
    let reserve_balance: i64 =
        sqlx::query_scalar("SELECT balance FROM insurance_reserve_state WHERE id = 1")
            .fetch_one(pool)
            .await?;
    let (open_deficits, outstanding, shortfall, covered, written_off): (i64, i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT
                COALESCE(SUM(status = 'open'), 0),
                COALESCE(SUM(shortfall - covered - written_off), 0),
                COALESCE(SUM(shortfall), 0),
                COALESCE(SUM(covered), 0),
                COALESCE(SUM(written_off), 0)
             FROM liquidation_deficits",
        )
        .fetch_one(pool)
        .await?;
    let net_reserve = reserve_balance - outstanding;
    Ok(CoverageReport {
        reserve_balance,
        open_deficits,
        outstanding_deficit: outstanding,
        net_reserve,
        total_shortfall: shortfall,
        total_covered: covered,
        total_written_off: written_off,
        at_risk_collateral,
        coverage_ratio_bps: ratio_bps(net_reserve, at_risk_collateral),
        recovery_rate_bps: ratio_bps(covered, shortfall),
    })
}

/// Checks the ledger invariants. Returns one message per violation; an
/// empty list means the reserve and deficit ledger reconcile.
pub async fn verify(pool: &SqlitePool) -> Result<Vec<String>, ReserveError> {
    let mut violations = Vec::new();

    let (balance, journal_sum, last_after): (i64, i64, Option<i64>) = sqlx::query_as(
        "SELECT
            (SELECT balance FROM insurance_reserve_state WHERE id = 1),
            (SELECT COALESCE(SUM(amount), 0) FROM insurance_reserve_entries),
            (SELECT balance_after FROM insurance_reserve_entries ORDER BY id DESC LIMIT 1)",
    )
    .fetch_one(pool)
    .await?;
    if balance != journal_sum {
        violations.push(format!(
            "reserve balance {balance} != journal sum {journal_sum}"
        ));
    }
    if let Some(after) = last_after {
        if after != balance {
            violations.push(format!(
                "reserve balance {balance} != last journal balance_after {after}"
            ));
        }
    }

    let mismatched: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT d.id, d.covered, COALESCE(-SUM(e.amount), 0)
           FROM liquidation_deficits d
           LEFT JOIN insurance_reserve_entries e
                  ON e.deficit_id = d.id AND e.kind = 'deficit_cover'
          GROUP BY d.id
         HAVING d.covered <> COALESCE(-SUM(e.amount), 0)",
    )
    .fetch_all(pool)
    .await?;
    for (id, covered, drawn) in mismatched {
        violations.push(format!(
            "deficit {id}: covered {covered} != cover draws {drawn}"
        ));
    }

    Ok(violations)
}

/// Collateral locked by open short positions, in stroops. Zero when the
/// positions table doesn't exist.
pub async fn open_short_collateral(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'positions'",
    )
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        return Ok(0);
    }
    let units: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(collateral), 0.0) FROM positions \
         WHERE status = 'open' AND position_type = 'short'",
    )
    .fetch_one(pool)
    .await?;
    Ok(to_stroops(units))
}

/// `GET /api/v1/insurance/coverage` — the public coverage metric.
pub async fn get_coverage(
    State(state): State<crate::AppState>,
) -> Result<Json<CoverageReport>, (StatusCode, String)> {
    let internal = |e: &dyn std::fmt::Display| {
        tracing::error!(error = %e, "insurance coverage report failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "coverage report unavailable".to_string(),
        )
    };
    let at_risk = open_short_collateral(&state.db)
        .await
        .map_err(|e| internal(&e))?;
    coverage(&state.db, at_risk)
        .await
        .map(Json)
        .map_err(|e| internal(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    const UNIT: i64 = STROOPS_PER_UNIT;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0012_insurance_reserve.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    fn shortfall<'a>(r: &'a str, amount: i64) -> Shortfall<'a> {
        Shortfall {
            liquidation_ref: r,
            account: "GACCOUNT",
            underlying: Some("XLM"),
            amount,
        }
    }

    async fn assert_reconciles(pool: &SqlitePool) {
        assert_eq!(verify(pool).await.unwrap(), Vec::<String>::new());
    }

    #[test]
    fn cover_amount_is_bounded_by_both_sides() {
        assert_eq!(cover_amount(100, 30), 30);
        assert_eq!(cover_amount(30, 100), 30);
        assert_eq!(cover_amount(0, 100), 0);
    }

    #[test]
    fn ratio_bps_handles_zero_and_large_values() {
        assert_eq!(ratio_bps(1, 0), None);
        assert_eq!(ratio_bps(50, 100), Some(5_000));
        assert_eq!(ratio_bps(-25, 100), Some(-2_500));
        assert_eq!(ratio_bps(i64::MAX, 1), Some(i64::MAX));
    }

    #[tokio::test]
    async fn fully_covered_shortfall_closes_the_deficit() {
        let pool = fresh_pool().await;
        contribute(&pool, InflowKind::Contribution, 100 * UNIT, None, None)
            .await
            .unwrap();

        let out = record_shortfall(&pool, &shortfall("liq-1", 40 * UNIT))
            .await
            .unwrap();
        assert_eq!(out.covered, 40 * UNIT);
        assert_eq!(out.outstanding, 0);

        let report = coverage(&pool, 0).await.unwrap();
        assert_eq!(report.reserve_balance, 60 * UNIT);
        assert_eq!(report.open_deficits, 0);
        assert_eq!(report.recovery_rate_bps, Some(10_000));
        assert_reconciles(&pool).await;
    }

    #[tokio::test]
    async fn uncovered_remainder_stays_open_and_visible() {
        let pool = fresh_pool().await;
        contribute(&pool, InflowKind::LiquidationPenalty, 10 * UNIT, None, None)
            .await
            .unwrap();

        let out = record_shortfall(&pool, &shortfall("liq-1", 25 * UNIT))
            .await
            .unwrap();
        assert_eq!(out.covered, 10 * UNIT);
        assert_eq!(out.outstanding, 15 * UNIT);

        let report = coverage(&pool, 300 * UNIT).await.unwrap();
        assert_eq!(report.reserve_balance, 0);
        assert_eq!(report.open_deficits, 1);
        assert_eq!(report.outstanding_deficit, 15 * UNIT);
        assert_eq!(report.net_reserve, -15 * UNIT);
        assert_eq!(report.coverage_ratio_bps, Some(-500));
        assert_eq!(report.recovery_rate_bps, Some(4_000));
        assert_reconciles(&pool).await;
    }

    #[tokio::test]
    async fn recording_the_same_liquidation_twice_draws_once() {
        let pool = fresh_pool().await;
        contribute(&pool, InflowKind::Contribution, 100 * UNIT, None, None)
            .await
            .unwrap();
        let first = record_shortfall(&pool, &shortfall("liq-1", 30 * UNIT))
            .await
            .unwrap();
        let again = record_shortfall(&pool, &shortfall("liq-1", 30 * UNIT))
            .await
            .unwrap();
        assert!(again.already_recorded);
        assert_eq!(again.deficit_id, first.deficit_id);
        assert_eq!(coverage(&pool, 0).await.unwrap().reserve_balance, 70 * UNIT);

        let conflict = record_shortfall(&pool, &shortfall("liq-1", 31 * UNIT)).await;
        assert!(matches!(
            conflict,
            Err(ReserveError::ConflictingShortfall { .. })
        ));
        assert_reconciles(&pool).await;
    }

    #[tokio::test]
    async fn later_contributions_cover_oldest_deficits_first() {
        let pool = fresh_pool().await;
        let a = record_shortfall(&pool, &shortfall("liq-a", 10 * UNIT))
            .await
            .unwrap();
        let b = record_shortfall(&pool, &shortfall("liq-b", 10 * UNIT))
            .await
            .unwrap();
        assert_eq!(a.outstanding, 10 * UNIT);

        contribute(&pool, InflowKind::FeeShare, 15 * UNIT, None, None)
            .await
            .unwrap();
        let applied = cover_open_deficits(&pool).await.unwrap();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].deficit_id, a.deficit_id);
        assert_eq!(applied[0].outstanding, 0);
        assert_eq!(applied[1].deficit_id, b.deficit_id);
        assert_eq!(applied[1].outstanding, 5 * UNIT);

        let report = coverage(&pool, 0).await.unwrap();
        assert_eq!(report.reserve_balance, 0);
        assert_eq!(report.open_deficits, 1);
        assert_reconciles(&pool).await;
    }

    #[tokio::test]
    async fn write_off_resolves_without_touching_the_reserve() {
        let pool = fresh_pool().await;
        let d = record_shortfall(&pool, &shortfall("liq-1", 10 * UNIT))
            .await
            .unwrap();
        let too_much = write_off(&pool, &d.deficit_id, Some(11 * UNIT), "x").await;
        assert!(matches!(
            too_much,
            Err(ReserveError::InvalidWriteOff { .. })
        ));

        let out = write_off(&pool, &d.deficit_id, None, "governance vote #12")
            .await
            .unwrap();
        assert_eq!(out.written_off, 10 * UNIT);
        assert_eq!(out.outstanding, 0);

        let status: String =
            sqlx::query_scalar("SELECT status FROM liquidation_deficits WHERE id = ?1")
                .bind(&d.deficit_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "written_off");
        let report = coverage(&pool, 0).await.unwrap();
        assert_eq!(report.total_written_off, 10 * UNIT);
        assert_eq!(report.recovery_rate_bps, Some(0));
        assert_reconciles(&pool).await;
    }

    #[tokio::test]
    async fn withdrawals_are_blocked_by_open_deficits_and_balance() {
        let pool = fresh_pool().await;
        contribute(&pool, InflowKind::Contribution, 5 * UNIT, None, None)
            .await
            .unwrap();
        record_shortfall(&pool, &shortfall("liq-1", 10 * UNIT))
            .await
            .unwrap();
        assert!(matches!(
            withdraw(&pool, UNIT, "ops").await,
            Err(ReserveError::OpenDeficits { .. })
        ));

        let pool = fresh_pool().await;
        contribute(&pool, InflowKind::Contribution, 5 * UNIT, None, None)
            .await
            .unwrap();
        assert!(matches!(
            withdraw(&pool, 6 * UNIT, "ops").await,
            Err(ReserveError::InsufficientReserve { .. })
        ));
        assert_eq!(withdraw(&pool, 2 * UNIT, "ops").await.unwrap(), 3 * UNIT);
        assert_reconciles(&pool).await;
    }

    #[tokio::test]
    async fn invalid_amounts_are_rejected() {
        let pool = fresh_pool().await;
        assert!(matches!(
            contribute(&pool, InflowKind::Contribution, 0, None, None).await,
            Err(ReserveError::InvalidAmount(0))
        ));
        assert!(matches!(
            record_shortfall(&pool, &shortfall("liq-1", -1)).await,
            Err(ReserveError::InvalidAmount(-1))
        ));
    }

    #[tokio::test]
    async fn journal_is_append_only_and_tampering_is_detected() {
        let pool = fresh_pool().await;
        contribute(&pool, InflowKind::Contribution, 5 * UNIT, None, None)
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE insurance_reserve_entries SET amount = 1")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(sqlx::query("DELETE FROM insurance_reserve_entries")
            .execute(&pool)
            .await
            .is_err());

        // A balance edited behind the journal's back is reported.
        sqlx::query("UPDATE insurance_reserve_state SET balance = balance + 1")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(verify(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn concurrent_shortfalls_never_overdraw_the_reserve() {
        let dir = std::env::temp_dir().join(format!("zenith-reserve-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.join("r.db").display());
        let opts: sqlx::sqlite::SqliteConnectOptions = url.parse().unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(
                opts.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                    .busy_timeout(std::time::Duration::from_secs(10)),
            )
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0012_insurance_reserve.sql"))
            .execute(&pool)
            .await
            .unwrap();
        contribute(&pool, InflowKind::Contribution, 50 * UNIT, None, None)
            .await
            .unwrap();

        let mut handles = Vec::new();
        for i in 0..20 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                let r = format!("liq-{i}");
                record_shortfall(&pool, &shortfall(&r, 5 * UNIT)).await
            }));
        }
        let mut covered = 0;
        for h in handles {
            covered += h.await.unwrap().unwrap().covered;
        }
        assert_eq!(covered, 50 * UNIT);
        let report = coverage(&pool, 0).await.unwrap();
        assert_eq!(report.reserve_balance, 0);
        assert_eq!(report.outstanding_deficit, 50 * UNIT);
        assert_reconciles(&pool).await;
        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
