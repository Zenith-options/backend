//! Detection and controlled recovery of chain writes that fail at the ledger
//! level.
//!
//! A chain write (e.g. "settle position 42 on-chain") can be accepted by RPC
//! and still never take effect:
//!
//! * **fee too low** – rejected with `txINSUFFICIENT_FEE` during surge pricing;
//! * **transaction set expiration** – the envelope's time bounds passed before
//!   it made it into a ledger (`txTOO_LATE`);
//! * **bad sequence** – another transaction used the source account's
//!   sequence number first (`txBAD_SEQ`);
//! * **tx_not_found** – accepted, then silently dropped, so lookups never find
//!   it;
//! * **rolled back** – reported as applied in ledger *N*, then no longer found
//!   (the RPC node we read from was on a bad fork or served stale data).
//!
//! Each write is a row in `chain_writes` keyed by a caller-chosen
//! `idempotency_key`, and [`ChainWriteRecovery::process`] walks every row
//! through `pending → submitted → confirmed → finalized`, sending failures
//! back to `pending` for a controlled replay. Three rules stop a replay from
//! duplicating ledger effects:
//!
//! 1. **Never replay while the old envelope can still land.** A submission is
//!    only declared lost once its envelope's time bound has passed (plus a
//!    grace period), so the original and the replay can't both be applied.
//! 2. **Probe before resubmitting.** Every hash ever submitted for a write is
//!    recorded in `chain_write_attempts`; all of them are looked up before a
//!    new envelope is built, and one that landed late is adopted instead.
//! 3. **Apply off-chain effects once per key.** Off-chain ledger effects run
//!    in the same database transaction that inserts the key into
//!    `chain_write_effects`. If the confirming ledger is later rolled back and
//!    the write is replayed, the second confirmation finds the key and skips
//!    them.
//!
//! Replays are controlled: fees are bumped only after `txINSUFFICIENT_FEE`
//! and capped at `max_fee_stroops`, retries back off exponentially, each pass
//! submits at most `max_submissions_per_pass` envelopes, the whole engine can
//! be paused, and a write that uses its attempt budget goes to `dead` for an
//! operator. Writes whose effects were applied but which ended up `failed` or
//! `dead` after a rollback are listed by [`ChainWriteRecovery::orphaned_effects`]
//! so they can be compensated.

use async_trait::async_trait;
use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ─── Failure classification ──────────────────────────────────────────────────

/// Why a chain write did not take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    FeeTooLow,
    TxSetExpired,
    BadSequence,
    NotFound,
    RolledBack,
    /// Node is overloaded or temporarily refusing (`TRY_AGAIN_LATER`).
    Transient,
    /// Anything replaying won't fix (bad auth, malformed, contract error).
    Permanent,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::FeeTooLow => "fee_too_low",
            FailureKind::TxSetExpired => "tx_set_expired",
            FailureKind::BadSequence => "bad_sequence",
            FailureKind::NotFound => "tx_not_found",
            FailureKind::RolledBack => "rolled_back",
            FailureKind::Transient => "transient",
            FailureKind::Permanent => "permanent",
        }
    }

    pub fn is_retryable(self) -> bool {
        self != FailureKind::Permanent
    }
}

/// Maps a Horizon / Soroban RPC result code to a [`FailureKind`]. Accepts
/// both the XDR spelling (`txINSUFFICIENT_FEE`) and the snake-case spelling
/// some APIs return (`tx_insufficient_fee`). Unknown codes are permanent, so a
/// new failure mode is surfaced rather than replayed forever.
pub fn classify_result_code(code: &str) -> FailureKind {
    let normalized: String = code
        .trim()
        .chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect();
    match normalized.as_str() {
        "txinsufficientfee" | "insufficientfee" | "feetoolow" => FailureKind::FeeTooLow,
        "txtoolate" | "txsetexpired" | "txexpired" => FailureKind::TxSetExpired,
        "txbadseq" => FailureKind::BadSequence,
        "txnotfound" | "notfound" => FailureKind::NotFound,
        "tryagainlater" | "txinternalerror" => FailureKind::Transient,
        _ => FailureKind::Permanent,
    }
}

// ─── Ports ───────────────────────────────────────────────────────────────────

/// A signed envelope ready to submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEnvelope {
    pub xdr: String,
    pub hash: String,
    /// Upper time bound of the envelope (Unix ms). After this the network
    /// will never include it.
    pub valid_until_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitResult {
    /// Accepted into the node's queue (RPC `PENDING`).
    Accepted,
    /// Already known to the node (RPC `DUPLICATE`) – treat as accepted.
    Duplicate,
    /// Rejected before inclusion, with the result code.
    Rejected { code: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxLookup {
    NotFound,
    Success {
        ledger: u32,
    },
    /// Included in a ledger but failed on-chain.
    Failed {
        ledger: u32,
        code: String,
    },
}

/// The network client. Errors mean "couldn't tell", never "failed".
#[async_trait]
pub trait LedgerClient: Send + Sync {
    async fn submit(&self, envelope: &SignedEnvelope) -> Result<SubmitResult, String>;
    async fn lookup(&self, tx_hash: &str) -> Result<TxLookup, String>;
    async fn latest_ledger(&self) -> Result<u32, String>;
}

/// Builds a fresh signed envelope for a write. Every call must use a current
/// sequence number and fresh time bounds; the contract call should carry the
/// write's idempotency key so the contract can reject a duplicate as well.
#[async_trait]
pub trait EnvelopeBuilder: Send + Sync {
    async fn build(
        &self,
        write: &ChainWrite,
        fee_stroops: i64,
        now_ms: i64,
    ) -> Result<SignedEnvelope, String>;
}

/// Applies a confirmed write's off-chain ledger effects. Runs inside the
/// transaction that records the effects as applied, so it's all-or-nothing
/// and runs at most once per idempotency key.
#[async_trait]
pub trait LedgerEffects: Send + Sync {
    async fn apply(
        &self,
        conn: &mut SqliteConnection,
        write: &ChainWrite,
        ledger: u32,
    ) -> Result<(), sqlx::Error>;
}

// ─── Records and config ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize)]
pub struct ChainWrite {
    pub id: String,
    pub idempotency_key: String,
    pub operation: String,
    pub payload: String,
    pub status: String,
    pub attempts: i64,
    pub attempt_budget: i64,
    pub fee_stroops: i64,
    pub current_tx_hash: Option<String>,
    pub deadline_ms: Option<i64>,
    pub confirmed_ledger: Option<i64>,
    pub last_failure_kind: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    pub base_fee_stroops: i64,
    pub max_fee_stroops: i64,
    pub fee_bump_multiplier: i64,
    pub max_attempts: i64,
    /// Extra wait after an envelope's time bound before declaring it lost.
    pub not_found_grace_ms: i64,
    /// How often a submitted write is looked up.
    pub poll_interval_ms: i64,
    /// Ledgers on top of the confirming one before a write is final.
    pub finality_depth: u32,
    pub base_backoff_ms: i64,
    pub max_backoff_ms: i64,
    pub max_submissions_per_pass: usize,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            base_fee_stroops: 100,
            max_fee_stroops: 1_000_000,
            fee_bump_multiplier: 2,
            max_attempts: 6,
            not_found_grace_ms: 10_000,
            poll_interval_ms: 5_000,
            finality_depth: 3,
            base_backoff_ms: 2_000,
            max_backoff_ms: 120_000,
            max_submissions_per_pass: 50,
        }
    }
}

impl RecoveryConfig {
    fn backoff_ms(&self, attempts: i64) -> i64 {
        let exp = (attempts.max(1) - 1).min(30) as u32;
        self.base_backoff_ms
            .saturating_mul(1i64 << exp)
            .min(self.max_backoff_ms)
    }

    fn next_fee(&self, current: i64, last_failure: Option<&str>) -> i64 {
        if last_failure == Some(FailureKind::FeeTooLow.as_str()) {
            current
                .saturating_mul(self.fee_bump_multiplier.max(1))
                .min(self.max_fee_stroops)
        } else {
            current
        }
    }
}

/// What a single [`ChainWriteRecovery::process`] pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryReport {
    pub submitted: usize,
    pub confirmed: usize,
    /// Confirmations that skipped effects because they were already applied.
    pub effects_deduplicated: usize,
    /// An earlier attempt was found to have landed and was adopted.
    pub adopted: usize,
    pub finalized: usize,
    pub rolled_back: usize,
    pub retries_scheduled: usize,
    pub failed: usize,
    pub dead: usize,
}

// ─── Engine ──────────────────────────────────────────────────────────────────

pub struct ChainWriteRecovery {
    pool: SqlitePool,
    client: Arc<dyn LedgerClient>,
    builder: Arc<dyn EnvelopeBuilder>,
    effects: Arc<dyn LedgerEffects>,
    config: RecoveryConfig,
    paused: AtomicBool,
}

impl ChainWriteRecovery {
    pub fn new(
        pool: SqlitePool,
        client: Arc<dyn LedgerClient>,
        builder: Arc<dyn EnvelopeBuilder>,
        effects: Arc<dyn LedgerEffects>,
        config: RecoveryConfig,
    ) -> Self {
        Self {
            pool,
            client,
            builder,
            effects,
            config,
            paused: AtomicBool::new(false),
        }
    }

    /// Stops new submissions and replays. Confirmation tracking continues.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        tracing::warn!(paused, "chain write replays paused flag changed");
    }

    /// Registers a write. Idempotent: registering the same key again returns
    /// the existing write's id and changes nothing.
    pub async fn register(
        &self,
        operation: &str,
        idempotency_key: &str,
        payload: &serde_json::Value,
        now_ms: i64,
    ) -> Result<String, sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO chain_writes
                 (id, idempotency_key, operation, payload, attempt_budget, fee_stroops,
                  next_attempt_at_ms, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?7)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(idempotency_key)
        .bind(operation)
        .bind(payload.to_string())
        .bind(self.config.max_attempts)
        .bind(self.config.base_fee_stroops)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        sqlx::query_scalar("SELECT id FROM chain_writes WHERE idempotency_key = ?")
            .bind(idempotency_key)
            .fetch_one(&self.pool)
            .await
    }

    pub async fn get(&self, id: &str) -> Result<Option<ChainWrite>, sqlx::Error> {
        sqlx::query_as(&format!("{SELECT_WRITE} WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// One recovery pass. Safe to run from several workers: every state change
    /// is guarded by the status it expects to move from.
    pub async fn process(&self, now_ms: i64) -> Result<RecoveryReport, sqlx::Error> {
        let mut report = RecoveryReport::default();
        self.check_confirmed(now_ms, &mut report).await?;
        self.check_submitted(now_ms, &mut report).await?;
        if !self.paused.load(Ordering::SeqCst) {
            self.submit_due(now_ms, &mut report).await?;
        }
        Ok(report)
    }

    /// Confirmed writes: finalize once deep enough, or detect a rollback.
    async fn check_confirmed(
        &self,
        now_ms: i64,
        report: &mut RecoveryReport,
    ) -> Result<(), sqlx::Error> {
        let writes: Vec<ChainWrite> =
            sqlx::query_as(&format!("{SELECT_WRITE} WHERE status = 'confirmed'"))
                .fetch_all(&self.pool)
                .await?;
        if writes.is_empty() {
            return Ok(());
        }
        let Ok(latest) = self.client.latest_ledger().await else {
            return Ok(());
        };
        for w in writes {
            let Some(hash) = w.current_tx_hash.clone() else {
                continue;
            };
            match self.client.lookup(&hash).await {
                Ok(TxLookup::Success { ledger }) => {
                    if latest >= ledger.saturating_add(self.config.finality_depth) {
                        if self
                            .transition(&w.id, "confirmed", "finalized", now_ms)
                            .await?
                        {
                            report.finalized += 1;
                        }
                    } else if w.confirmed_ledger != Some(ledger as i64) {
                        sqlx::query(
                            "UPDATE chain_writes SET confirmed_ledger = ?, updated_at_ms = ?
                              WHERE id = ? AND status = 'confirmed'",
                        )
                        .bind(ledger as i64)
                        .bind(now_ms)
                        .bind(&w.id)
                        .execute(&self.pool)
                        .await?;
                    }
                }
                Ok(TxLookup::NotFound) | Ok(TxLookup::Failed { .. }) => {
                    // The ledger we confirmed against no longer has it. Go back
                    // to `submitted` until the envelope's time bound passes: it
                    // may yet be re-included, and replaying before then could
                    // apply the operation twice on-chain.
                    let valid_until = self.valid_until(&w.id, &hash).await?;
                    let deadline = valid_until.max(now_ms) + self.config.not_found_grace_ms;
                    let n = sqlx::query(
                        "UPDATE chain_writes
                            SET status = 'submitted', confirmed_ledger = NULL, deadline_ms = ?,
                                last_failure_kind = ?, last_error = ?,
                                next_attempt_at_ms = ?, updated_at_ms = ?
                          WHERE id = ? AND status = 'confirmed'",
                    )
                    .bind(deadline)
                    .bind(FailureKind::RolledBack.as_str())
                    .bind(format!(
                        "{hash} no longer found after confirmation at ledger {:?}",
                        w.confirmed_ledger
                    ))
                    .bind(now_ms)
                    .bind(now_ms)
                    .bind(&w.id)
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
                    if n == 1 {
                        tracing::error!(write = %w.id, tx = %hash, ledger = ?w.confirmed_ledger,
                            "confirmed chain write rolled back");
                        report.rolled_back += 1;
                    }
                }
                Err(e) => tracing::debug!(write = %w.id, error = %e, "lookup failed, will retry"),
            }
        }
        Ok(())
    }

    /// Submitted writes: confirm, fail, or declare lost after the deadline.
    async fn check_submitted(
        &self,
        now_ms: i64,
        report: &mut RecoveryReport,
    ) -> Result<(), sqlx::Error> {
        let writes: Vec<ChainWrite> = sqlx::query_as(&format!(
            "{SELECT_WRITE} WHERE status = 'submitted' AND next_attempt_at_ms <= ?"
        ))
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await?;

        for w in writes {
            let Some(hash) = w.current_tx_hash.clone() else {
                continue;
            };
            match self.client.lookup(&hash).await {
                Ok(TxLookup::Success { ledger }) => {
                    self.confirm(&w, "submitted", &hash, ledger, now_ms, report)
                        .await?;
                }
                Ok(TxLookup::Failed { code, .. }) => {
                    self.record_failure(
                        &w,
                        "submitted",
                        classify_result_code(&code),
                        &code,
                        now_ms,
                        report,
                    )
                    .await?;
                }
                Ok(TxLookup::NotFound) => {
                    if let Some(deadline) = w.deadline_ms.filter(|d| now_ms < *d) {
                        // Next check no later than the deadline itself.
                        let at = (now_ms + self.config.poll_interval_ms).min(deadline);
                        self.poll_at(&w.id, at).await?;
                        continue;
                    }
                    // Lost. An earlier attempt may still have landed.
                    match self.probe_attempts(&w.id).await {
                        Some(Ok((h, ledger))) => {
                            self.confirm(&w, "submitted", &h, ledger, now_ms, report)
                                .await?;
                            report.adopted += 1;
                        }
                        Some(Err(())) => {
                            self.poll_at(&w.id, now_ms + self.config.poll_interval_ms)
                                .await?
                        }
                        None => {
                            let msg = format!("{hash} not found after deadline");
                            self.record_failure(
                                &w,
                                "submitted",
                                FailureKind::NotFound,
                                &msg,
                                now_ms,
                                report,
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => tracing::debug!(write = %w.id, error = %e, "lookup failed, will retry"),
            }
        }
        Ok(())
    }

    /// Pending writes that are due: probe old attempts, then (re)submit.
    async fn submit_due(
        &self,
        now_ms: i64,
        report: &mut RecoveryReport,
    ) -> Result<(), sqlx::Error> {
        let writes: Vec<ChainWrite> = sqlx::query_as(&format!(
            "{SELECT_WRITE} WHERE status = 'pending' AND next_attempt_at_ms <= ?
              ORDER BY next_attempt_at_ms LIMIT ?"
        ))
        .bind(now_ms)
        .bind(self.config.max_submissions_per_pass as i64)
        .fetch_all(&self.pool)
        .await?;

        for w in writes {
            if w.attempts > 0 {
                match self.probe_attempts(&w.id).await {
                    Some(Ok((h, ledger))) => {
                        self.confirm(&w, "pending", &h, ledger, now_ms, report)
                            .await?;
                        report.adopted += 1;
                        continue;
                    }
                    // Can't rule out an earlier landing; don't risk a duplicate.
                    Some(Err(())) => continue,
                    None => {}
                }
            }
            if w.attempts >= w.attempt_budget {
                if self.transition(&w.id, "pending", "dead", now_ms).await? {
                    tracing::error!(write = %w.id, key = %w.idempotency_key, attempts = w.attempts,
                        "chain write out of attempts, parked as dead");
                    report.dead += 1;
                }
                continue;
            }

            let fee = self
                .config
                .next_fee(w.fee_stroops, w.last_failure_kind.as_deref());
            let envelope = match self.builder.build(&w, fee, now_ms).await {
                Ok(env) => env,
                Err(e) => {
                    self.reschedule(
                        &w.id,
                        FailureKind::Transient,
                        &e,
                        now_ms + self.config.backoff_ms(w.attempts.max(1)),
                        now_ms,
                    )
                    .await?;
                    continue;
                }
            };
            let attempt = w.attempts + 1;

            // Record the hash *before* sending so it is always probed later,
            // even if we crash between submit and the status update.
            let claimed = {
                let mut tx = self.pool.begin().await?;
                let n = sqlx::query(
                    "UPDATE chain_writes
                        SET status = 'submitted', attempts = ?1, fee_stroops = ?2, current_tx_hash = ?3,
                            deadline_ms = ?4, next_attempt_at_ms = ?5, updated_at_ms = ?6
                      WHERE id = ?7 AND status = 'pending' AND attempts = ?8",
                )
                .bind(attempt)
                .bind(fee)
                .bind(&envelope.hash)
                .bind(envelope.valid_until_ms + self.config.not_found_grace_ms)
                .bind(now_ms + self.config.poll_interval_ms)
                .bind(now_ms)
                .bind(&w.id)
                .bind(w.attempts)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if n == 1 {
                    sqlx::query(
                        "INSERT INTO chain_write_attempts
                             (write_id, attempt, tx_hash, fee_stroops, valid_until_ms, submitted_at_ms)
                         VALUES (?, ?, ?, ?, ?, ?)",
                    )
                    .bind(&w.id)
                    .bind(attempt)
                    .bind(&envelope.hash)
                    .bind(fee)
                    .bind(envelope.valid_until_ms)
                    .bind(now_ms)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                n == 1
            };
            if !claimed {
                continue; // another worker got it
            }

            match self.client.submit(&envelope).await {
                Ok(SubmitResult::Accepted) | Ok(SubmitResult::Duplicate) => {
                    report.submitted += 1;
                    if w.attempts > 0 {
                        tracing::info!(write = %w.id, attempt, fee, tx = %envelope.hash,
                            reason = ?w.last_failure_kind, "replayed chain write");
                    }
                }
                Ok(SubmitResult::Rejected { code }) => {
                    let mut w = w.clone();
                    w.attempts = attempt;
                    w.fee_stroops = fee;
                    self.set_attempt_outcome(&w.id, attempt, &code).await?;
                    self.record_failure(
                        &w,
                        "submitted",
                        classify_result_code(&code),
                        &code,
                        now_ms,
                        report,
                    )
                    .await?;
                }
                // The request may or may not have reached the network. Leave the
                // write `submitted`; lookups will tell us after the deadline.
                Err(e) => {
                    report.submitted += 1;
                    tracing::warn!(write = %w.id, error = %e, "submit outcome unknown, tracking by hash");
                }
            }
        }
        Ok(())
    }

    /// Marks a write confirmed and applies its effects at most once.
    async fn confirm(
        &self,
        w: &ChainWrite,
        from_status: &str,
        hash: &str,
        ledger: u32,
        now_ms: i64,
        report: &mut RecoveryReport,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let moved = sqlx::query(
            "UPDATE chain_writes
                SET status = 'confirmed', current_tx_hash = ?, confirmed_ledger = ?,
                    deadline_ms = NULL, updated_at_ms = ?
              WHERE id = ? AND status = ?",
        )
        .bind(hash)
        .bind(ledger as i64)
        .bind(now_ms)
        .bind(&w.id)
        .bind(from_status)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved == 0 {
            return Ok(()); // raced with another worker; drop the transaction
        }
        let first_time = sqlx::query(
            "INSERT OR IGNORE INTO chain_write_effects
                 (idempotency_key, write_id, tx_hash, ledger, applied_at_ms)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&w.idempotency_key)
        .bind(&w.id)
        .bind(hash)
        .bind(ledger as i64)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if first_time {
            self.effects.apply(&mut tx, w, ledger).await?;
        } else {
            report.effects_deduplicated += 1;
        }
        sqlx::query("UPDATE chain_write_attempts SET outcome = 'success' WHERE write_id = ? AND tx_hash = ?")
            .bind(&w.id)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        report.confirmed += 1;
        Ok(())
    }

    async fn record_failure(
        &self,
        w: &ChainWrite,
        from_status: &str,
        kind: FailureKind,
        detail: &str,
        now_ms: i64,
        report: &mut RecoveryReport,
    ) -> Result<(), sqlx::Error> {
        if kind.is_retryable() {
            // Bad-sequence and fee errors are cheap to fix, so retry quickly.
            let delay = match kind {
                FailureKind::FeeTooLow | FailureKind::BadSequence | FailureKind::TxSetExpired => {
                    self.config.base_backoff_ms
                }
                _ => self.config.backoff_ms(w.attempts),
            };
            let n = sqlx::query(
                "UPDATE chain_writes
                    SET status = 'pending', last_failure_kind = ?, last_error = ?,
                        next_attempt_at_ms = ?, deadline_ms = NULL, updated_at_ms = ?
                  WHERE id = ? AND status = ?",
            )
            .bind(kind.as_str())
            .bind(detail)
            .bind(now_ms + delay)
            .bind(now_ms)
            .bind(&w.id)
            .bind(from_status)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if n == 1 {
                tracing::warn!(write = %w.id, kind = kind.as_str(), detail, "chain write failed, replay scheduled");
                report.retries_scheduled += 1;
            }
        } else {
            let n = sqlx::query(
                "UPDATE chain_writes
                    SET status = 'failed', last_failure_kind = ?, last_error = ?, updated_at_ms = ?
                  WHERE id = ? AND status = ?",
            )
            .bind(kind.as_str())
            .bind(detail)
            .bind(now_ms)
            .bind(&w.id)
            .bind(from_status)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if n == 1 {
                tracing::error!(write = %w.id, detail, "chain write failed permanently");
                report.failed += 1;
            }
        }
        Ok(())
    }

    /// Looks up every hash ever submitted for a write. `Some(Ok)` if one
    /// landed, `None` if none did, `Some(Err)` if a lookup failed and we
    /// can't tell.
    async fn probe_attempts(&self, write_id: &str) -> Option<Result<(String, u32), ()>> {
        let hashes: Vec<String> = sqlx::query_scalar(
            "SELECT tx_hash FROM chain_write_attempts WHERE write_id = ? ORDER BY attempt DESC",
        )
        .bind(write_id)
        .fetch_all(&self.pool)
        .await
        .ok()?;
        let mut unknown = false;
        for h in hashes {
            match self.client.lookup(&h).await {
                Ok(TxLookup::Success { ledger }) => return Some(Ok((h, ledger))),
                Ok(_) => {}
                Err(_) => unknown = true,
            }
        }
        unknown.then_some(Err(()))
    }

    async fn valid_until(&self, write_id: &str, hash: &str) -> Result<i64, sqlx::Error> {
        let v: Option<i64> = sqlx::query_scalar(
            "SELECT valid_until_ms FROM chain_write_attempts WHERE write_id = ? AND tx_hash = ?",
        )
        .bind(write_id)
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(v.unwrap_or(0))
    }

    async fn transition(
        &self,
        id: &str,
        from: &str,
        to: &str,
        now_ms: i64,
    ) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE chain_writes SET status = ?, updated_at_ms = ? WHERE id = ? AND status = ?",
        )
        .bind(to)
        .bind(now_ms)
        .bind(id)
        .bind(from)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    async fn poll_at(&self, id: &str, at_ms: i64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE chain_writes SET next_attempt_at_ms = ? WHERE id = ?")
            .bind(at_ms)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn reschedule(
        &self,
        id: &str,
        kind: FailureKind,
        err: &str,
        at_ms: i64,
        now_ms: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE chain_writes SET last_failure_kind = ?, last_error = ?, next_attempt_at_ms = ?, updated_at_ms = ?
              WHERE id = ? AND status = 'pending'",
        )
        .bind(kind.as_str())
        .bind(err)
        .bind(at_ms)
        .bind(now_ms)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_attempt_outcome(
        &self,
        id: &str,
        attempt: i64,
        outcome: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE chain_write_attempts SET outcome = ? WHERE write_id = ? AND attempt = ?",
        )
        .bind(outcome)
        .bind(id)
        .bind(attempt)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Operator action: gives a dead write a fresh attempt budget.
    pub async fn retry_dead(&self, id: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE chain_writes
                SET status = 'pending', attempt_budget = attempts + ?, next_attempt_at_ms = ?, updated_at_ms = ?
              WHERE id = ? AND status = 'dead'",
        )
        .bind(self.config.max_attempts)
        .bind(now_ms)
        .bind(now_ms)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Writes whose off-chain effects were applied but that ended up failed or
    /// dead (only possible after a rollback). These need compensation.
    pub async fn orphaned_effects(&self) -> Result<Vec<ChainWrite>, sqlx::Error> {
        sqlx::query_as(&format!(
            "{SELECT_WRITE} WHERE status IN ('failed', 'dead')
               AND idempotency_key IN (SELECT idempotency_key FROM chain_write_effects)"
        ))
        .fetch_all(&self.pool)
        .await
    }
}

const SELECT_WRITE: &str =
    "SELECT id, idempotency_key, operation, payload, status, attempts, attempt_budget,
        fee_stroops, current_tx_hash, deadline_ms, confirmed_ledger, last_failure_kind, last_error
   FROM chain_writes";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    const VALIDITY_MS: i64 = 30_000;

    #[derive(Default)]
    struct FakeLedger {
        submits: Mutex<VecDeque<SubmitResult>>,
        txs: Mutex<HashMap<String, TxLookup>>,
        sent: Mutex<Vec<String>>,
        latest: Mutex<u32>,
        /// Accepted envelopes land in the next ledger immediately.
        land: AtomicBool,
    }

    impl FakeLedger {
        fn script(&self, results: impl IntoIterator<Item = SubmitResult>) {
            self.submits.lock().unwrap().extend(results);
        }
        fn set(&self, hash: &str, status: TxLookup) {
            self.txs.lock().unwrap().insert(hash.to_string(), status);
        }
        fn remove(&self, hash: &str) {
            self.txs.lock().unwrap().remove(hash);
        }
        fn advance(&self, ledgers: u32) {
            *self.latest.lock().unwrap() += ledgers;
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LedgerClient for FakeLedger {
        async fn submit(&self, env: &SignedEnvelope) -> Result<SubmitResult, String> {
            self.sent.lock().unwrap().push(env.hash.clone());
            let r = self
                .submits
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(SubmitResult::Accepted);
            if r == SubmitResult::Accepted && self.land.load(Ordering::SeqCst) {
                let ledger = *self.latest.lock().unwrap() + 1;
                self.set(&env.hash, TxLookup::Success { ledger });
            }
            Ok(r)
        }
        async fn lookup(&self, hash: &str) -> Result<TxLookup, String> {
            Ok(self
                .txs
                .lock()
                .unwrap()
                .get(hash)
                .cloned()
                .unwrap_or(TxLookup::NotFound))
        }
        async fn latest_ledger(&self) -> Result<u32, String> {
            Ok(*self.latest.lock().unwrap())
        }
    }

    struct FakeBuilder;

    #[async_trait]
    impl EnvelopeBuilder for FakeBuilder {
        async fn build(
            &self,
            w: &ChainWrite,
            fee: i64,
            now_ms: i64,
        ) -> Result<SignedEnvelope, String> {
            Ok(SignedEnvelope {
                xdr: format!("xdr:{}", w.idempotency_key),
                hash: format!("{}#{}@{fee}", w.idempotency_key, w.attempts + 1),
                valid_until_ms: now_ms + VALIDITY_MS,
            })
        }
    }

    /// Credits `payload.amount` to `payload.wallet`.
    struct CreditEffects;

    #[async_trait]
    impl LedgerEffects for CreditEffects {
        async fn apply(
            &self,
            conn: &mut SqliteConnection,
            w: &ChainWrite,
            _ledger: u32,
        ) -> Result<(), sqlx::Error> {
            let p: serde_json::Value = serde_json::from_str(&w.payload).unwrap();
            sqlx::query(
                "INSERT INTO test_balances (wallet, amount) VALUES (?1, ?2)
                 ON CONFLICT(wallet) DO UPDATE SET amount = amount + ?2",
            )
            .bind(p["wallet"].as_str().unwrap())
            .bind(p["amount"].as_i64().unwrap())
            .execute(conn)
            .await?;
            Ok(())
        }
    }

    async fn setup(config: RecoveryConfig) -> (ChainWriteRecovery, Arc<FakeLedger>, SqlitePool) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0013_chain_write_recovery.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE test_balances (wallet TEXT PRIMARY KEY, amount INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let ledger = Arc::new(FakeLedger::default());
        *ledger.latest.lock().unwrap() = 100;
        let engine = ChainWriteRecovery::new(
            pool.clone(),
            ledger.clone(),
            Arc::new(FakeBuilder),
            Arc::new(CreditEffects),
            config,
        );
        (engine, ledger, pool)
    }

    async fn balance(pool: &SqlitePool, wallet: &str) -> i64 {
        sqlx::query_scalar("SELECT amount FROM test_balances WHERE wallet = ?")
            .bind(wallet)
            .fetch_optional(pool)
            .await
            .unwrap()
            .unwrap_or(0)
    }

    async fn register(engine: &ChainWriteRecovery, key: &str) -> String {
        engine
            .register("settle", key, &json!({ "wallet": "GA1", "amount": 500 }), 0)
            .await
            .unwrap()
    }

    #[test]
    fn classifies_ledger_result_codes() {
        assert_eq!(
            classify_result_code("txINSUFFICIENT_FEE"),
            FailureKind::FeeTooLow
        );
        assert_eq!(
            classify_result_code("tx_insufficient_fee"),
            FailureKind::FeeTooLow
        );
        assert_eq!(
            classify_result_code("txTOO_LATE"),
            FailureKind::TxSetExpired
        );
        assert_eq!(
            classify_result_code("tx_set_expired"),
            FailureKind::TxSetExpired
        );
        assert_eq!(classify_result_code("txBAD_SEQ"), FailureKind::BadSequence);
        assert_eq!(classify_result_code("tx_not_found"), FailureKind::NotFound);
        assert_eq!(
            classify_result_code("TRY_AGAIN_LATER"),
            FailureKind::Transient
        );
        assert_eq!(classify_result_code("txBAD_AUTH"), FailureKind::Permanent);
        assert_eq!(classify_result_code("txFAILED"), FailureKind::Permanent);
        assert_eq!(
            classify_result_code("something_new"),
            FailureKind::Permanent
        );
    }

    #[test]
    fn fee_bumps_only_after_fee_too_low_and_caps() {
        let c = RecoveryConfig {
            max_fee_stroops: 300,
            ..RecoveryConfig::default()
        };
        assert_eq!(c.next_fee(100, None), 100);
        assert_eq!(c.next_fee(100, Some("tx_not_found")), 100);
        assert_eq!(c.next_fee(100, Some("fee_too_low")), 200);
        assert_eq!(c.next_fee(200, Some("fee_too_low")), 300);
    }

    #[tokio::test]
    async fn register_is_idempotent() {
        let (engine, _, _) = setup(RecoveryConfig::default()).await;
        let a = register(&engine, "settle:1").await;
        let b = register(&engine, "settle:1").await;
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn happy_path_confirms_applies_once_and_finalizes() {
        let (engine, ledger, pool) = setup(RecoveryConfig::default()).await;
        ledger.land.store(true, Ordering::SeqCst);
        let id = register(&engine, "settle:1").await;

        assert_eq!(engine.process(0).await.unwrap().submitted, 1);
        let r = engine.process(5_000).await.unwrap();
        assert_eq!(r.confirmed, 1);
        assert_eq!(balance(&pool, "GA1").await, 500);

        // Confirmed in ledger 101; three ledgers on top of it is 104.
        ledger.advance(3);
        assert_eq!(engine.process(9_000).await.unwrap().finalized, 0);
        ledger.advance(1);
        assert_eq!(engine.process(10_000).await.unwrap().finalized, 1);
        assert_eq!(engine.get(&id).await.unwrap().unwrap().status, "finalized");
        assert_eq!(balance(&pool, "GA1").await, 500);
    }

    #[tokio::test]
    async fn fee_too_low_is_replayed_with_bumped_fee() {
        let (engine, ledger, pool) = setup(RecoveryConfig::default()).await;
        ledger.land.store(true, Ordering::SeqCst);
        ledger.script([SubmitResult::Rejected {
            code: "txINSUFFICIENT_FEE".into(),
        }]);
        let id = register(&engine, "settle:1").await;

        let r = engine.process(0).await.unwrap();
        assert_eq!(r.retries_scheduled, 1);
        let w = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(w.status, "pending");
        assert_eq!(w.last_failure_kind.as_deref(), Some("fee_too_low"));

        engine.process(2_000).await.unwrap();
        engine.process(7_000).await.unwrap();
        let w = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(w.status, "confirmed");
        assert_eq!(w.fee_stroops, 200);
        assert_eq!(ledger.sent(), vec!["settle:1#1@100", "settle:1#2@200"]);
        assert_eq!(balance(&pool, "GA1").await, 500);
    }

    #[tokio::test]
    async fn expired_tx_set_is_rebuilt_and_resubmitted() {
        let (engine, ledger, _) = setup(RecoveryConfig::default()).await;
        ledger.land.store(true, Ordering::SeqCst);
        ledger.script([SubmitResult::Rejected {
            code: "txTOO_LATE".into(),
        }]);
        let id = register(&engine, "settle:1").await;

        engine.process(0).await.unwrap();
        engine.process(2_000).await.unwrap();
        engine.process(7_000).await.unwrap();
        let w = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(w.status, "confirmed");
        assert_eq!(w.fee_stroops, 100, "expiry does not bump the fee");
        assert_eq!(ledger.sent().len(), 2);
    }

    #[tokio::test]
    async fn not_found_is_only_replayed_after_envelope_expires() {
        let config = RecoveryConfig::default();
        let deadline = VALIDITY_MS + config.not_found_grace_ms;
        let (engine, ledger, pool) = setup(config).await;
        let id = register(&engine, "settle:1").await;

        engine.process(0).await.unwrap(); // accepted, never lands
        let r = engine.process(deadline - 1).await.unwrap();
        assert_eq!(r.retries_scheduled, 0, "envelope could still land");
        assert_eq!(ledger.sent().len(), 1);

        let r = engine.process(deadline).await.unwrap();
        assert_eq!(r.retries_scheduled, 1);
        let w = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(w.last_failure_kind.as_deref(), Some("tx_not_found"));

        ledger.land.store(true, Ordering::SeqCst);
        let replay_at = deadline + RecoveryConfig::default().backoff_ms(1);
        assert_eq!(engine.process(replay_at).await.unwrap().submitted, 1);
        assert_eq!(
            engine.process(replay_at + 5_000).await.unwrap().confirmed,
            1
        );
        assert_eq!(balance(&pool, "GA1").await, 500);
    }

    #[tokio::test]
    async fn late_landing_attempt_is_adopted_instead_of_replayed() {
        let config = RecoveryConfig::default();
        let deadline = VALIDITY_MS + config.not_found_grace_ms;
        let (engine, ledger, pool) = setup(config).await;
        let id = register(&engine, "settle:1").await;

        engine.process(0).await.unwrap();
        engine.process(deadline).await.unwrap(); // declared lost, replay scheduled
                                                 // The node we read from was lagging: attempt 1 did land.
        ledger.set("settle:1#1@100", TxLookup::Success { ledger: 99 });

        let r = engine.process(deadline + 60_000).await.unwrap();
        assert_eq!((r.adopted, r.confirmed, r.submitted), (1, 1, 0));
        assert_eq!(ledger.sent().len(), 1, "no duplicate submission");
        assert_eq!(
            engine
                .get(&id)
                .await
                .unwrap()
                .unwrap()
                .current_tx_hash
                .as_deref(),
            Some("settle:1#1@100")
        );
        assert_eq!(balance(&pool, "GA1").await, 500);
    }

    #[tokio::test]
    async fn rolled_back_write_is_replayed_without_duplicating_effects() {
        let config = RecoveryConfig::default();
        let grace = config.not_found_grace_ms;
        let (engine, ledger, pool) = setup(config).await;
        ledger.land.store(true, Ordering::SeqCst);
        let id = register(&engine, "settle:1").await;

        engine.process(0).await.unwrap();
        engine.process(5_000).await.unwrap();
        assert_eq!(balance(&pool, "GA1").await, 500);

        // The confirming ledger is rolled back.
        ledger.land.store(false, Ordering::SeqCst);
        ledger.remove("settle:1#1@100");
        let r = engine.process(6_000).await.unwrap();
        assert_eq!(r.rolled_back, 1);
        let w = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(
            (w.status.as_str(), w.last_failure_kind.as_deref()),
            ("submitted", Some("rolled_back"))
        );

        // Not replayed while the original envelope is still valid.
        engine.process(VALIDITY_MS).await.unwrap();
        assert_eq!(ledger.sent().len(), 1);

        let lost_at = VALIDITY_MS + grace;
        assert_eq!(engine.process(lost_at).await.unwrap().retries_scheduled, 1);
        ledger.land.store(true, Ordering::SeqCst);
        let replay_at = lost_at + 60_000;
        engine.process(replay_at).await.unwrap();
        let r = engine.process(replay_at + 5_000).await.unwrap();
        assert_eq!((r.confirmed, r.effects_deduplicated), (1, 1));
        assert_eq!(
            balance(&pool, "GA1").await,
            500,
            "effects applied exactly once"
        );

        ledger.advance(10);
        engine.process(replay_at + 10_000).await.unwrap();
        assert_eq!(engine.get(&id).await.unwrap().unwrap().status, "finalized");
        assert!(engine.orphaned_effects().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn permanent_failures_are_not_replayed() {
        let (engine, ledger, pool) = setup(RecoveryConfig::default()).await;
        ledger.script([SubmitResult::Rejected {
            code: "txBAD_AUTH".into(),
        }]);
        let id = register(&engine, "settle:1").await;

        assert_eq!(engine.process(0).await.unwrap().failed, 1);
        engine.process(600_000).await.unwrap();
        assert_eq!(ledger.sent().len(), 1);
        assert_eq!(engine.get(&id).await.unwrap().unwrap().status, "failed");
        assert_eq!(balance(&pool, "GA1").await, 0);
    }

    #[tokio::test]
    async fn on_chain_failure_is_not_replayed() {
        let (engine, ledger, _) = setup(RecoveryConfig::default()).await;
        let id = register(&engine, "settle:1").await;
        engine.process(0).await.unwrap();
        ledger.set(
            "settle:1#1@100",
            TxLookup::Failed {
                ledger: 101,
                code: "txFAILED".into(),
            },
        );
        assert_eq!(engine.process(5_000).await.unwrap().failed, 1);
        assert_eq!(engine.get(&id).await.unwrap().unwrap().status, "failed");
    }

    #[tokio::test]
    async fn attempt_budget_dead_letters_and_operator_can_retry() {
        let config = RecoveryConfig {
            max_attempts: 2,
            ..RecoveryConfig::default()
        };
        let (engine, ledger, _) = setup(config).await;
        ledger.script(vec![
            SubmitResult::Rejected {
                code: "txBAD_SEQ".into()
            };
            2
        ]);
        let id = register(&engine, "settle:1").await;

        engine.process(0).await.unwrap();
        engine.process(10_000).await.unwrap();
        assert_eq!(engine.process(20_000).await.unwrap().dead, 1);
        assert_eq!(engine.get(&id).await.unwrap().unwrap().status, "dead");

        ledger.land.store(true, Ordering::SeqCst);
        assert!(engine.retry_dead(&id, 30_000).await.unwrap());
        engine.process(30_000).await.unwrap();
        engine.process(35_000).await.unwrap();
        assert_eq!(engine.get(&id).await.unwrap().unwrap().status, "confirmed");
        assert_eq!(ledger.sent().len(), 3);
    }

    #[tokio::test]
    async fn paused_engine_does_not_submit() {
        let (engine, ledger, _) = setup(RecoveryConfig::default()).await;
        register(&engine, "settle:1").await;
        engine.set_paused(true);
        engine.process(0).await.unwrap();
        assert!(ledger.sent().is_empty());
        engine.set_paused(false);
        assert_eq!(engine.process(0).await.unwrap().submitted, 1);
    }

    #[tokio::test]
    async fn submissions_per_pass_are_capped() {
        let config = RecoveryConfig {
            max_submissions_per_pass: 2,
            ..RecoveryConfig::default()
        };
        let (engine, ledger, _) = setup(config).await;
        for i in 0..5 {
            register(&engine, &format!("settle:{i}")).await;
        }
        engine.process(0).await.unwrap();
        assert_eq!(ledger.sent().len(), 2);
    }
}
