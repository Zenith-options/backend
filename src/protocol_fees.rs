//! Protocol fees on premiums and settlement payouts, and treasury sweeps.
//!
//! **Accrual.** When a premium is paid or a settlement payout is made, the
//! protocol keeps a fee out of the gross amount: the counterparty receives
//! `gross - fee` and the fee is recorded in `protocol_fee_accruals`.
//! [`accrue`] takes a database connection so callers record the fee in the
//! same transaction as the trade or payout it belongs to. Each
//! `(source, source_id)` is charged at most once, so retrying a payout never
//! charges twice.
//!
//! **Arithmetic.** Amounts are integer stroops. `fee = ceil(gross * bps /
//! 10_000)`, floored at `min_fee` and never more than `gross`. Rounding is up,
//! matching the protocol-favouring rounding proved for `math_fixed::fee_fixed`.
//! Intermediates are `i128`, so no `i64` amount can overflow.
//!
//! **Sweeps.** [`TreasurySweeper::sweep_all`] periodically moves accrued fees
//! to the configured on-chain treasury account, one sweep per asset:
//!
//! 1. In one transaction, create a sweep and claim every unswept
//!    accrual for the asset by setting its `sweep_id`. A partial unique index
//!    allows only one open sweep per asset, so concurrent sweepers can't send
//!    the same fees twice.
//! 2. Ask the [`TreasuryTransfer`] to send the total, using the sweep id as
//!    the idempotency key / memo.
//! 3. On confirmation the sweep is `confirmed` and its accruals stay attached.
//!    On rejection the sweep is `failed` and its accruals are released for the
//!    next sweep. If the outcome is unknown (timeout, crash) the sweep stays
//!    `submitted` and later passes look it up by sweep id; it is only failed
//!    once the transfer reports it definitively not found.
//!
//! [`fee_summary`] reports accrued, in-flight and swept totals per asset for
//! reconciliation against the treasury's on-chain balance.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::sync::Arc;

use crate::strkey::{validate_stellar_address, AddressType};

/// Fee rates above this are rejected as a configuration error.
pub const MAX_FEE_BPS: u32 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeSource {
    Premium,
    SettlementPayout,
}

impl FeeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FeeSource::Premium => "premium",
            FeeSource::SettlementPayout => "settlement_payout",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeConfigError {
    RateTooHigh { bps: u32 },
    NegativeMinimum,
    InvalidTreasury(String),
    NonPositiveSweepMinimum,
}

impl std::fmt::Display for FeeConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeeConfigError::RateTooHigh { bps } => {
                write!(f, "fee rate {bps}bps exceeds the {MAX_FEE_BPS}bps limit")
            }
            FeeConfigError::NegativeMinimum => write!(f, "minimum fee must not be negative"),
            FeeConfigError::InvalidTreasury(e) => write!(f, "invalid treasury account: {e}"),
            FeeConfigError::NonPositiveSweepMinimum => {
                write!(f, "minimum sweep amount must be positive")
            }
        }
    }
}

impl std::error::Error for FeeConfigError {}

/// Fee rates per source. Construct with [`FeeSchedule::new`] to validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FeeSchedule {
    premium_bps: u32,
    settlement_payout_bps: u32,
    min_fee: i64,
}

impl FeeSchedule {
    pub fn new(
        premium_bps: u32,
        settlement_payout_bps: u32,
        min_fee: i64,
    ) -> Result<Self, FeeConfigError> {
        for bps in [premium_bps, settlement_payout_bps] {
            if bps > MAX_FEE_BPS {
                return Err(FeeConfigError::RateTooHigh { bps });
            }
        }
        if min_fee < 0 {
            return Err(FeeConfigError::NegativeMinimum);
        }
        Ok(Self {
            premium_bps,
            settlement_payout_bps,
            min_fee,
        })
    }

    pub fn bps(&self, source: FeeSource) -> u32 {
        match source {
            FeeSource::Premium => self.premium_bps,
            FeeSource::SettlementPayout => self.settlement_payout_bps,
        }
    }

    pub fn fee(&self, source: FeeSource, gross: i64) -> i64 {
        compute_fee(gross, self.bps(source), self.min_fee)
    }
}

/// `ceil(gross * bps / 10_000)`, at least `min_fee` (when the rate is
/// non-zero) and never more than `gross`. Zero or negative gross is fee-free.
pub fn compute_fee(gross: i64, bps: u32, min_fee: i64) -> i64 {
    if gross <= 0 || bps == 0 {
        return 0;
    }
    let raw = (gross as i128 * bps as i128 + 9_999) / 10_000;
    (raw as i64).max(min_fee).min(gross)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Accrual {
    pub gross: i64,
    pub fee: i64,
    /// What the counterparty receives.
    pub net: i64,
    /// False if this source was already charged; the stored fee is returned.
    pub newly_accrued: bool,
}

/// Records the protocol fee for a premium or payout. Idempotent per
/// `(source, source_id)`: a retry returns the originally charged fee.
pub async fn accrue(
    conn: &mut SqliteConnection,
    schedule: &FeeSchedule,
    source: FeeSource,
    source_id: &str,
    asset: &str,
    gross: i64,
    now_ms: i64,
) -> Result<Accrual, sqlx::Error> {
    let gross = gross.max(0);
    let fee = schedule.fee(source, gross);
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO protocol_fee_accruals
             (id, source, source_id, asset, gross_amount, fee_amount, fee_bps, created_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(source.as_str())
    .bind(source_id)
    .bind(asset)
    .bind(gross)
    .bind(fee)
    .bind(schedule.bps(source) as i64)
    .bind(now_ms)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;

    if inserted {
        return Ok(Accrual {
            gross,
            fee,
            net: gross - fee,
            newly_accrued: true,
        });
    }
    let (gross, fee): (i64, i64) = sqlx::query_as(
        "SELECT gross_amount, fee_amount FROM protocol_fee_accruals
          WHERE source = ? AND source_id = ?",
    )
    .bind(source.as_str())
    .bind(source_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Accrual {
        gross,
        fee,
        net: gross - fee,
        newly_accrued: false,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AssetFeeSummary {
    pub asset: String,
    /// Accrued and not yet claimed by a sweep.
    pub accrued: i64,
    /// Claimed by a pending or submitted sweep.
    pub in_flight: i64,
    /// Delivered to the treasury.
    pub swept: i64,
}

/// Per-asset fee totals, for dashboards and treasury reconciliation.
pub async fn fee_summary(pool: &SqlitePool) -> Result<Vec<AssetFeeSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT a.asset,
                COALESCE(SUM(CASE WHEN a.sweep_id IS NULL THEN a.fee_amount END), 0) AS accrued,
                COALESCE(SUM(CASE WHEN s.status IN ('pending', 'submitted') THEN a.fee_amount END), 0) AS in_flight,
                COALESCE(SUM(CASE WHEN s.status = 'confirmed' THEN a.fee_amount END), 0) AS swept
           FROM protocol_fee_accruals a
           LEFT JOIN treasury_sweeps s ON s.id = a.sweep_id
          GROUP BY a.asset
          ORDER BY a.asset",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AssetFeeSummary {
            asset: r.get("asset"),
            accrued: r.get("accrued"),
            in_flight: r.get("in_flight"),
            swept: r.get("swept"),
        })
        .collect())
}

// ─── Treasury sweeps ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferResult {
    Confirmed { tx_hash: String },
    Rejected { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferLookup {
    Confirmed {
        tx_hash: String,
    },
    /// Definitively never landed and can no longer land.
    NotFound,
    /// Still in flight or can't tell yet.
    Unknown,
}

/// Sends fees on-chain. `sweep_id` must be used as the idempotency key (for
/// example as the memo, or as the idempotency key of a
/// [`crate::chain_recovery`] write), so a retried transfer can't pay twice.
#[async_trait]
pub trait TreasuryTransfer: Send + Sync {
    /// `Err` means the outcome is unknown.
    async fn transfer(
        &self,
        sweep_id: &str,
        asset: &str,
        amount: i64,
        destination: &str,
    ) -> Result<TransferResult, String>;

    async fn lookup(&self, sweep_id: &str) -> Result<TransferLookup, String>;
}

#[derive(Debug, Clone)]
pub struct SweepConfig {
    treasury_account: String,
    /// Don't sweep an asset until at least this much has accrued.
    pub min_sweep_amount: i64,
}

impl SweepConfig {
    /// The treasury must be a valid Stellar account (`G...`) or contract
    /// (`C...`) address.
    pub fn new(treasury_account: &str, min_sweep_amount: i64) -> Result<Self, FeeConfigError> {
        match validate_stellar_address(treasury_account) {
            Ok(AddressType::Ed25519PublicKey | AddressType::Contract) => {}
            Ok(AddressType::MuxedAccount) => {
                return Err(FeeConfigError::InvalidTreasury(
                    "muxed accounts are not supported as treasury".into(),
                ))
            }
            Err(e) => return Err(FeeConfigError::InvalidTreasury(e)),
        }
        if min_sweep_amount <= 0 {
            return Err(FeeConfigError::NonPositiveSweepMinimum);
        }
        Ok(Self {
            treasury_account: treasury_account.to_string(),
            min_sweep_amount,
        })
    }

    pub fn treasury_account(&self) -> &str {
        &self.treasury_account
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SweepOutcome {
    Confirmed {
        sweep_id: String,
        asset: String,
        amount: i64,
        tx_hash: String,
    },
    Failed {
        sweep_id: String,
        asset: String,
        amount: i64,
        reason: String,
    },
    /// Outcome unknown; will be looked up on the next pass.
    InFlight {
        sweep_id: String,
        asset: String,
        amount: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize)]
pub struct TreasurySweep {
    pub id: String,
    pub asset: String,
    pub amount: i64,
    pub accrual_count: i64,
    pub treasury_account: String,
    pub status: String,
    pub tx_hash: Option<String>,
    pub last_error: Option<String>,
}

pub struct TreasurySweeper {
    pool: SqlitePool,
    transfer: Arc<dyn TreasuryTransfer>,
    config: SweepConfig,
}

impl TreasurySweeper {
    pub fn new(pool: SqlitePool, transfer: Arc<dyn TreasuryTransfer>, config: SweepConfig) -> Self {
        Self {
            pool,
            transfer,
            config,
        }
    }

    /// One sweep pass: resolve sweeps with unknown outcomes, then sweep every
    /// asset whose accrued balance has reached the minimum.
    pub async fn sweep_all(&self, now_ms: i64) -> Result<Vec<SweepOutcome>, sqlx::Error> {
        let mut out = self.resolve_in_flight(now_ms).await?;

        let assets: Vec<String> = sqlx::query_scalar(
            "SELECT asset FROM protocol_fee_accruals
              WHERE sweep_id IS NULL
              GROUP BY asset
             HAVING SUM(fee_amount) >= ?
              ORDER BY asset",
        )
        .bind(self.config.min_sweep_amount)
        .fetch_all(&self.pool)
        .await?;

        for asset in assets {
            if let Some(o) = self.sweep_asset(&asset, now_ms).await? {
                out.push(o);
            }
        }
        Ok(out)
    }

    async fn sweep_asset(
        &self,
        asset: &str,
        now_ms: i64,
    ) -> Result<Option<SweepOutcome>, sqlx::Error> {
        let sweep_id = uuid::Uuid::new_v4().to_string();

        // Create the sweep and claim the accruals atomically.
        let mut tx = self.pool.begin().await?;
        let created = sqlx::query(
            "INSERT OR IGNORE INTO treasury_sweeps
                 (id, asset, treasury_account, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?4)",
        )
        .bind(&sweep_id)
        .bind(asset)
        .bind(self.config.treasury_account())
        .bind(now_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if created == 0 {
            return Ok(None); // another sweep for this asset is still open
        }
        sqlx::query(
            "UPDATE protocol_fee_accruals SET sweep_id = ? WHERE asset = ? AND sweep_id IS NULL",
        )
        .bind(&sweep_id)
        .bind(asset)
        .execute(&mut *tx)
        .await?;
        let (amount, count): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(fee_amount), 0), COUNT(*) FROM protocol_fee_accruals WHERE sweep_id = ?",
        )
        .bind(&sweep_id)
        .fetch_one(&mut *tx)
        .await?;
        if amount < self.config.min_sweep_amount {
            tx.rollback().await?;
            return Ok(None);
        }
        sqlx::query(
            "UPDATE treasury_sweeps SET amount = ?, accrual_count = ?, status = 'submitted'
              WHERE id = ?",
        )
        .bind(amount)
        .bind(count)
        .bind(&sweep_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let result = self
            .transfer
            .transfer(&sweep_id, asset, amount, self.config.treasury_account())
            .await;
        let asset = asset.to_string();
        Ok(Some(match result {
            Ok(TransferResult::Confirmed { tx_hash }) => {
                self.mark_confirmed(&sweep_id, &tx_hash, now_ms).await?;
                tracing::info!(sweep = %sweep_id, %asset, amount, tx = %tx_hash, "treasury sweep confirmed");
                SweepOutcome::Confirmed {
                    sweep_id,
                    asset,
                    amount,
                    tx_hash,
                }
            }
            Ok(TransferResult::Rejected { reason }) => {
                self.mark_failed(&sweep_id, &reason, now_ms).await?;
                SweepOutcome::Failed {
                    sweep_id,
                    asset,
                    amount,
                    reason,
                }
            }
            Err(e) => {
                tracing::warn!(sweep = %sweep_id, error = %e, "treasury sweep outcome unknown");
                sqlx::query(
                    "UPDATE treasury_sweeps SET last_error = ?, updated_at_ms = ? WHERE id = ?",
                )
                .bind(&e)
                .bind(now_ms)
                .bind(&sweep_id)
                .execute(&self.pool)
                .await?;
                SweepOutcome::InFlight {
                    sweep_id,
                    asset,
                    amount,
                }
            }
        }))
    }

    async fn resolve_in_flight(&self, now_ms: i64) -> Result<Vec<SweepOutcome>, sqlx::Error> {
        let open: Vec<TreasurySweep> = sqlx::query_as(
            "SELECT id, asset, amount, accrual_count, treasury_account, status, tx_hash, last_error
               FROM treasury_sweeps WHERE status = 'submitted'",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for s in open {
            match self.transfer.lookup(&s.id).await {
                Ok(TransferLookup::Confirmed { tx_hash }) => {
                    self.mark_confirmed(&s.id, &tx_hash, now_ms).await?;
                    out.push(SweepOutcome::Confirmed {
                        sweep_id: s.id,
                        asset: s.asset,
                        amount: s.amount,
                        tx_hash,
                    });
                }
                Ok(TransferLookup::NotFound) => {
                    let reason = "transfer not found".to_string();
                    self.mark_failed(&s.id, &reason, now_ms).await?;
                    out.push(SweepOutcome::Failed {
                        sweep_id: s.id,
                        asset: s.asset,
                        amount: s.amount,
                        reason,
                    });
                }
                Ok(TransferLookup::Unknown) | Err(_) => out.push(SweepOutcome::InFlight {
                    sweep_id: s.id,
                    asset: s.asset,
                    amount: s.amount,
                }),
            }
        }
        Ok(out)
    }

    async fn mark_confirmed(
        &self,
        id: &str,
        tx_hash: &str,
        now_ms: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE treasury_sweeps SET status = 'confirmed', tx_hash = ?, last_error = NULL, updated_at_ms = ?
              WHERE id = ? AND status IN ('pending', 'submitted')",
        )
        .bind(tx_hash)
        .bind(now_ms)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fails a sweep and releases its accruals for the next one.
    async fn mark_failed(&self, id: &str, reason: &str, now_ms: i64) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE treasury_sweeps SET status = 'failed', last_error = ?, updated_at_ms = ?
              WHERE id = ? AND status IN ('pending', 'submitted')",
        )
        .bind(reason)
        .bind(now_ms)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 1 {
            sqlx::query("UPDATE protocol_fee_accruals SET sweep_id = NULL WHERE sweep_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tracing::warn!(sweep = %id, reason, "treasury sweep failed, fees released");
        }
        tx.commit().await
    }

    pub async fn get(&self, id: &str) -> Result<Option<TreasurySweep>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, asset, amount, accrual_count, treasury_account, status, tx_hash, last_error
               FROM treasury_sweeps WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }
}

/// Runs [`TreasurySweeper::sweep_all`] every `interval`.
pub async fn run_sweep_loop(sweeper: Arc<TreasurySweeper>, interval: std::time::Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Err(e) = sweeper.sweep_all(now_ms).await {
            tracing::error!(error = %e, "treasury sweep pass failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strkey::encode_stellar_public_key;
    use std::sync::Mutex;

    fn treasury() -> String {
        encode_stellar_public_key(&[7u8; 32])
    }

    fn schedule() -> FeeSchedule {
        FeeSchedule::new(30, 10, 1).unwrap()
    }

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0014_protocol_fees.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    async fn charge(
        pool: &SqlitePool,
        source: FeeSource,
        id: &str,
        asset: &str,
        gross: i64,
    ) -> Accrual {
        let mut conn = pool.acquire().await.unwrap();
        accrue(&mut conn, &schedule(), source, id, asset, gross, 0)
            .await
            .unwrap()
    }

    #[derive(Default)]
    struct FakeTreasury {
        results: Mutex<Vec<Result<TransferResult, String>>>,
        lookups: Mutex<Vec<TransferLookup>>,
        sent: Mutex<Vec<(String, String, i64, String)>>,
    }

    #[async_trait]
    impl TreasuryTransfer for FakeTreasury {
        async fn transfer(
            &self,
            id: &str,
            asset: &str,
            amount: i64,
            dest: &str,
        ) -> Result<TransferResult, String> {
            self.sent
                .lock()
                .unwrap()
                .push((id.into(), asset.into(), amount, dest.into()));
            let mut r = self.results.lock().unwrap();
            if r.is_empty() {
                Ok(TransferResult::Confirmed {
                    tx_hash: format!("tx-{id}"),
                })
            } else {
                r.remove(0)
            }
        }
        async fn lookup(&self, _id: &str) -> Result<TransferLookup, String> {
            let mut l = self.lookups.lock().unwrap();
            Ok(if l.is_empty() {
                TransferLookup::Unknown
            } else {
                l.remove(0)
            })
        }
    }

    fn sweeper(pool: &SqlitePool, t: Arc<FakeTreasury>, min: i64) -> TreasurySweeper {
        TreasurySweeper::new(pool.clone(), t, SweepConfig::new(&treasury(), min).unwrap())
    }

    #[test]
    fn fee_rounds_up_in_favour_of_the_protocol() {
        assert_eq!(compute_fee(10_000, 30, 0), 30);
        assert_eq!(compute_fee(10_001, 30, 0), 31); // 30.003 -> 31
        assert_eq!(compute_fee(1, 1, 0), 1);
        assert_eq!(compute_fee(0, 30, 5), 0);
        assert_eq!(compute_fee(-50, 30, 5), 0);
        assert_eq!(
            compute_fee(1_000, 0, 5),
            0,
            "zero rate means no fee, even with a minimum"
        );
    }

    #[test]
    fn fee_respects_minimum_and_never_exceeds_gross() {
        assert_eq!(compute_fee(100, 30, 5), 5);
        assert_eq!(compute_fee(3, 30, 5), 3);
    }

    #[test]
    fn fee_does_not_overflow_on_extreme_amounts() {
        let fee = compute_fee(i64::MAX, MAX_FEE_BPS, 0);
        assert_eq!(fee, ((i64::MAX as i128 * 1_000 + 9_999) / 10_000) as i64);
    }

    #[test]
    fn schedule_and_sweep_config_are_validated() {
        assert_eq!(
            FeeSchedule::new(1_001, 0, 0),
            Err(FeeConfigError::RateTooHigh { bps: 1_001 })
        );
        assert_eq!(
            FeeSchedule::new(0, 0, -1),
            Err(FeeConfigError::NegativeMinimum)
        );
        assert!(matches!(
            SweepConfig::new("GNOTANADDRESS", 1),
            Err(FeeConfigError::InvalidTreasury(_))
        ));
        assert_eq!(
            SweepConfig::new(&treasury(), 0).unwrap_err(),
            FeeConfigError::NonPositiveSweepMinimum
        );
        let contract = crate::strkey::encode_contract_id(&[1u8; 32]);
        assert!(SweepConfig::new(&contract, 1).is_ok());
    }

    #[tokio::test]
    async fn accrues_on_premiums_and_payouts_once() {
        let pool = pool().await;
        let a = charge(&pool, FeeSource::Premium, "pos-1", "USDC", 1_000_000).await;
        assert_eq!((a.fee, a.net, a.newly_accrued), (3_000, 997_000, true));
        let b = charge(&pool, FeeSource::SettlementPayout, "pos-1", "USDC", 500_000).await;
        assert_eq!(b.fee, 500);

        let again = charge(&pool, FeeSource::Premium, "pos-1", "USDC", 1_000_000).await;
        assert_eq!((again.fee, again.newly_accrued), (3_000, false));

        let s = fee_summary(&pool).await.unwrap();
        assert_eq!(
            s,
            vec![AssetFeeSummary {
                asset: "USDC".into(),
                accrued: 3_500,
                in_flight: 0,
                swept: 0
            }]
        );
    }

    #[tokio::test]
    async fn accrual_rolls_back_with_its_trade() {
        let pool = pool().await;
        let mut tx = pool.begin().await.unwrap();
        accrue(
            &mut tx,
            &schedule(),
            FeeSource::Premium,
            "pos-9",
            "USDC",
            1_000_000,
            0,
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        assert!(fee_summary(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_moves_accrued_fees_to_treasury() {
        let pool = pool().await;
        charge(&pool, FeeSource::Premium, "p1", "USDC", 1_000_000).await;
        charge(&pool, FeeSource::Premium, "p2", "USDC", 2_000_000).await;
        charge(&pool, FeeSource::Premium, "p3", "XLM", 1_000_000).await;
        let t = Arc::new(FakeTreasury::default());
        let sweeper = sweeper(&pool, t.clone(), 1);

        let out = sweeper.sweep_all(10).await.unwrap();
        assert_eq!(out.len(), 2);
        let sent = t.sent.lock().unwrap().clone();
        assert_eq!(
            sent.iter().map(|s| (s.1.as_str(), s.2)).collect::<Vec<_>>(),
            [("USDC", 9_000), ("XLM", 3_000)]
        );
        assert!(sent.iter().all(|s| s.3 == treasury()));

        let summary = fee_summary(&pool).await.unwrap();
        assert!(summary.iter().all(|s| s.accrued == 0 && s.in_flight == 0));
        assert_eq!(summary.iter().map(|s| s.swept).sum::<i64>(), 12_000);

        // Nothing left: the next pass sends nothing.
        assert!(sweeper.sweep_all(20).await.unwrap().is_empty());
        assert_eq!(t.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sweep_waits_for_minimum_amount() {
        let pool = pool().await;
        charge(&pool, FeeSource::Premium, "p1", "USDC", 1_000_000).await; // fee 3_000
        let t = Arc::new(FakeTreasury::default());
        let sweeper = sweeper(&pool, t.clone(), 5_000);
        assert!(sweeper.sweep_all(0).await.unwrap().is_empty());
        charge(&pool, FeeSource::Premium, "p2", "USDC", 1_000_000).await;
        assert_eq!(sweeper.sweep_all(1).await.unwrap().len(), 1);
        assert_eq!(t.sent.lock().unwrap()[0].2, 6_000);
    }

    #[tokio::test]
    async fn rejected_sweep_releases_fees_for_next_pass() {
        let pool = pool().await;
        charge(&pool, FeeSource::Premium, "p1", "USDC", 1_000_000).await;
        let t = Arc::new(FakeTreasury::default());
        t.results.lock().unwrap().push(Ok(TransferResult::Rejected {
            reason: "underfunded".into(),
        }));
        let sweeper = sweeper(&pool, t.clone(), 1);

        let out = sweeper.sweep_all(0).await.unwrap();
        assert!(matches!(
            &out[0],
            SweepOutcome::Failed { amount: 3_000, .. }
        ));
        assert_eq!(fee_summary(&pool).await.unwrap()[0].accrued, 3_000);

        let out = sweeper.sweep_all(1).await.unwrap();
        assert!(matches!(
            &out[0],
            SweepOutcome::Confirmed { amount: 3_000, .. }
        ));
        assert_eq!(fee_summary(&pool).await.unwrap()[0].swept, 3_000);
    }

    #[tokio::test]
    async fn unknown_outcome_blocks_new_sweep_until_resolved() {
        let pool = pool().await;
        charge(&pool, FeeSource::Premium, "p1", "USDC", 1_000_000).await;
        let t = Arc::new(FakeTreasury::default());
        t.results.lock().unwrap().push(Err("timeout".into()));
        let sweeper = sweeper(&pool, t.clone(), 1);

        let out = sweeper.sweep_all(0).await.unwrap();
        let SweepOutcome::InFlight { sweep_id, .. } = &out[0] else {
            panic!("{out:?}")
        };
        let sweep_id = sweep_id.clone();

        // New fees accrue but can't be swept while the first is unresolved.
        charge(&pool, FeeSource::Premium, "p2", "USDC", 1_000_000).await;
        let out = sweeper.sweep_all(1).await.unwrap();
        assert_eq!(
            out,
            vec![SweepOutcome::InFlight {
                sweep_id: sweep_id.clone(),
                asset: "USDC".into(),
                amount: 3_000
            }]
        );
        assert_eq!(
            t.sent.lock().unwrap().len(),
            1,
            "no second transfer while outcome unknown"
        );
        let s = &fee_summary(&pool).await.unwrap()[0];
        assert_eq!((s.accrued, s.in_flight), (3_000, 3_000));

        // It turns out the transfer landed.
        t.lookups.lock().unwrap().push(TransferLookup::Confirmed {
            tx_hash: "abc".into(),
        });
        let out = sweeper.sweep_all(2).await.unwrap();
        assert_eq!(out.len(), 2, "resolved sweep + new sweep");
        assert_eq!(
            sweeper
                .get(&sweep_id)
                .await
                .unwrap()
                .unwrap()
                .tx_hash
                .as_deref(),
            Some("abc")
        );
        let s = &fee_summary(&pool).await.unwrap()[0];
        assert_eq!((s.accrued, s.in_flight, s.swept), (0, 0, 6_000));
    }

    #[tokio::test]
    async fn not_found_transfer_is_failed_and_released() {
        let pool = pool().await;
        charge(&pool, FeeSource::Premium, "p1", "USDC", 1_000_000).await;
        let t = Arc::new(FakeTreasury::default());
        t.results.lock().unwrap().push(Err("timeout".into()));
        t.lookups.lock().unwrap().push(TransferLookup::NotFound);
        let sweeper = sweeper(&pool, t.clone(), 1);

        sweeper.sweep_all(0).await.unwrap();
        let out = sweeper.sweep_all(1).await.unwrap();
        assert!(matches!(&out[0], SweepOutcome::Failed { .. }));
        assert!(matches!(
            &out[1],
            SweepOutcome::Confirmed { amount: 3_000, .. }
        ));
        assert_eq!(t.sent.lock().unwrap().len(), 2);
    }
}
