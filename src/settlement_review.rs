//! Settlement batch previews and a human approval queue.
//!
//! Settlement batches move real collateral unattended. This module puts a
//! brake between "the keeper decided to settle" and "funds moved":
//!
//! 1. **Preview.** [`build_preview`] computes, for one
//!    `(underlying, expires_at)` and fixing price, exactly what the batch
//!    will do: per leg (payoff, collateral unlocked, shortfall) and per
//!    wallet (net cash change). All amounts are integer stroops, and the
//!    whole preview is hashed (`sha256`).
//! 2. **Policy.** [`ApprovalPolicy`] decides whether a human must look at
//!    it: large gross value, a large single-wallet credit, any shortfall,
//!    or a fallback/manual fixing all require review. Everything else is
//!    auto-approved, and that decision is recorded too.
//! 3. **Queue.** [`propose`] records the batch. [`approve`] requires a
//!    different operator from the proposer (four-eyes) and the hash of the
//!    preview they reviewed. Approvals expire.
//! 4. **Execute.** [`execute`] recomputes the preview from live data and
//!    refuses (`stale`) unless the hash still matches. What executes is
//!    byte-for-byte what was reviewed. It also re-checks the settlement
//!    contract's pinned hash and screens every wallet in the batch through
//!    the compliance gate before claiming the batch and calling the
//!    executor.
//!
//! Every transition is written to the append-only `settlement_batch_events`
//! table. The preview columns are immutable and terminal states are final
//! (both enforced by triggers in `migrations/0014_settlement_review.sql`).

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::amounts::{to_stroops, AmountError, STROOPS_PER_UNIT};
use crate::compliance::{ComplianceError, ComplianceGate, Flow};
use crate::contract_pins::{PinError, PinGuard, WasmHashSource};

// ─── Preview ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixingSource {
    /// TWAP over persisted ticks: the normal path.
    Twap,
    /// Too few ticks, so the last aggregated price was used. Always reviewed by default.
    Fallback,
    /// Entered by an operator. Always reviewed by default.
    Manual,
}

impl FixingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Twap => "twap",
            Self::Fallback => "fallback",
            Self::Manual => "manual",
        }
    }
}

/// An open position to be settled, with amounts already in stroops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementLeg {
    pub position_id: String,
    pub wallet: String,
    pub is_call: bool,
    pub is_short: bool,
    /// Contracts scaled by 10^7, so fractional contracts are exact.
    pub contracts: i64,
    pub strike: i64,
    /// Collateral locked against this leg (shorts only).
    pub collateral: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegOutcome {
    pub position_id: String,
    pub wallet: String,
    pub option_type: String,
    pub side: String,
    pub contracts: i64,
    pub strike: i64,
    /// Intrinsic value owed on this leg at the fixing.
    pub payoff: i64,
    /// Collateral unlocked (shorts: all of it).
    pub collateral_unlocked: i64,
    /// Portion of the unlocked collateral used to pay the payoff.
    pub paid_from_collateral: i64,
    /// Payoff the collateral could not cover.
    pub shortfall: i64,
    /// Change in the wallet's free balance.
    pub cash_delta: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletImpact {
    pub wallet: String,
    pub positions: i64,
    pub cash_delta: i64,
    pub collateral_unlocked: i64,
    pub shortfall: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewTotals {
    pub positions: i64,
    pub wallets: i64,
    /// Paid to long holders.
    pub long_payout: i64,
    /// Paid by short writers out of their collateral.
    pub short_paid: i64,
    pub collateral_unlocked: i64,
    pub shortfall: i64,
    /// What the platform counterparty pays (positive) or keeps (negative):
    /// `long_payout - short_paid`.
    pub platform_net: i64,
    /// Total value this batch moves: long payouts plus collateral unlocked.
    pub gross_value: i64,
    /// Largest single-wallet credit to free balance.
    pub max_wallet_credit: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchPreview {
    pub underlying: String,
    pub expires_at: String,
    pub fixing_price: i64,
    pub fixing_source: FixingSource,
    pub legs: Vec<LegOutcome>,
    pub wallets: Vec<WalletImpact>,
    pub totals: PreviewTotals,
    /// SHA-256 of the canonical JSON of every other field.
    pub sha256: String,
}

/// `a * b / 10^7`, rounded half up. Both inputs are non-negative here.
fn mul_scaled(a: i64, b: i64) -> i64 {
    let scale = STROOPS_PER_UNIT as i128;
    let product = a as i128 * b as i128;
    ((product + scale / 2) / scale) as i64
}

/// Pure preview computation. Legs are sorted by position ID first, so the
/// result (and its hash) does not depend on input order.
pub fn compute_preview(
    underlying: &str,
    expires_at: &str,
    fixing_price: i64,
    fixing_source: FixingSource,
    mut legs: Vec<SettlementLeg>,
) -> BatchPreview {
    legs.sort_by(|a, b| a.position_id.cmp(&b.position_id));
    let mut outcomes = Vec::with_capacity(legs.len());
    let mut wallets: BTreeMap<String, WalletImpact> = BTreeMap::new();
    let mut t = PreviewTotals::default();

    for leg in legs {
        let intrinsic = if leg.is_call {
            (fixing_price - leg.strike).max(0)
        } else {
            (leg.strike - fixing_price).max(0)
        };
        let payoff = mul_scaled(intrinsic, leg.contracts.abs());
        let (collateral_unlocked, paid, shortfall, cash_delta) = if leg.is_short {
            let paid = payoff.min(leg.collateral);
            (leg.collateral, paid, payoff - paid, leg.collateral - paid)
        } else {
            (0, 0, 0, payoff)
        };
        if leg.is_short {
            t.short_paid += paid;
        } else {
            t.long_payout += payoff;
        }
        t.collateral_unlocked += collateral_unlocked;
        t.shortfall += shortfall;
        t.positions += 1;

        let w = wallets
            .entry(leg.wallet.clone())
            .or_insert_with(|| WalletImpact {
                wallet: leg.wallet.clone(),
                positions: 0,
                cash_delta: 0,
                collateral_unlocked: 0,
                shortfall: 0,
            });
        w.positions += 1;
        w.cash_delta += cash_delta;
        w.collateral_unlocked += collateral_unlocked;
        w.shortfall += shortfall;

        outcomes.push(LegOutcome {
            position_id: leg.position_id,
            wallet: leg.wallet,
            option_type: if leg.is_call { "call" } else { "put" }.into(),
            side: if leg.is_short { "short" } else { "long" }.into(),
            contracts: leg.contracts,
            strike: leg.strike,
            payoff,
            collateral_unlocked,
            paid_from_collateral: paid,
            shortfall,
            cash_delta,
        });
    }
    let wallets: Vec<WalletImpact> = wallets.into_values().collect();
    t.wallets = wallets.len() as i64;
    t.platform_net = t.long_payout - t.short_paid;
    t.gross_value = t.long_payout + t.collateral_unlocked;
    t.max_wallet_credit = wallets
        .iter()
        .map(|w| w.cash_delta)
        .max()
        .unwrap_or(0)
        .max(0);

    let mut preview = BatchPreview {
        underlying: underlying.into(),
        expires_at: expires_at.into(),
        fixing_price,
        fixing_source,
        legs: outcomes,
        wallets,
        totals: t,
        sha256: String::new(),
    };
    preview.sha256 = preview_digest(&preview);
    preview
}

fn preview_digest(preview: &BatchPreview) -> String {
    let mut unsigned = preview.clone();
    unsigned.sha256.clear();
    let canonical = serde_json::to_vec(&unsigned).expect("preview serializes");
    Sha256::digest(&canonical)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Loads every open position for `(underlying, expires_at)` and computes
/// the preview. Used both when proposing and, again, right before execution.
pub async fn build_preview(
    pool: &SqlitePool,
    underlying: &str,
    expires_at: &str,
    fixing_price: i64,
    fixing_source: FixingSource,
) -> Result<BatchPreview, ReviewError> {
    let rows: Vec<(String, String, String, String, f64, f64, f64)> = sqlx::query_as(
        "SELECT id, wallet_address, option_type, position_type, CAST(contracts AS REAL), \
                CAST(strike AS REAL), CAST(collateral AS REAL) \
         FROM positions WHERE status = 'open' AND underlying = ? AND expires_at = ? ORDER BY id",
    )
    .bind(underlying)
    .bind(expires_at)
    .fetch_all(pool)
    .await?;
    let mut legs = Vec::with_capacity(rows.len());
    for (id, wallet, option_type, side, contracts, strike, collateral) in rows {
        let amount = |what: &str, v: f64| {
            to_stroops(v).map_err(|e: AmountError| {
                ReviewError::InvalidData(format!("position {id} {what}: {e}"))
            })
        };
        legs.push(SettlementLeg {
            contracts: amount("contracts", contracts)?,
            strike: amount("strike", strike)?,
            collateral: amount("collateral", collateral)?,
            position_id: id,
            wallet,
            is_call: option_type == "call",
            is_short: side == "short",
        });
    }
    Ok(compute_preview(
        underlying,
        expires_at,
        fixing_price,
        fixing_source,
        legs,
    ))
}

// ─── Policy ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ApprovalPolicy {
    /// Batches moving more than this (stroops) need a human.
    pub auto_approve_max_gross: i64,
    /// Any single wallet credited more than this (stroops) needs a human.
    pub auto_approve_max_wallet_credit: i64,
    pub review_on_shortfall: bool,
    pub review_on_non_twap_fixing: bool,
    /// Every batch needs a human (e.g. for the first weeks on mainnet).
    pub always_review: bool,
    /// How long an approval (or auto-approval) stays executable.
    pub approval_ttl_secs: i64,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            auto_approve_max_gross: 10_000 * STROOPS_PER_UNIT,
            auto_approve_max_wallet_credit: 2_500 * STROOPS_PER_UNIT,
            review_on_shortfall: true,
            review_on_non_twap_fixing: true,
            always_review: false,
            approval_ttl_secs: 60 * 60,
        }
    }
}

impl ApprovalPolicy {
    /// Reads `ZENITH_SETTLEMENT_AUTO_APPROVE_MAX` and
    /// `ZENITH_SETTLEMENT_AUTO_APPROVE_MAX_WALLET` (whole units),
    /// `ZENITH_SETTLEMENT_ALWAYS_REVIEW` (`true`/`false`) and
    /// `ZENITH_SETTLEMENT_APPROVAL_TTL_SECS`.
    pub fn from_env() -> Result<Self, String> {
        let mut p = Self::default();
        let units = |key: &str| -> Result<Option<i64>, String> {
            match std::env::var(key) {
                Ok(v) => v
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|n| *n >= 0.0)
                    .and_then(|n| to_stroops(n).ok())
                    .map(Some)
                    .ok_or_else(|| format!("{key} must be a non-negative amount")),
                Err(_) => Ok(None),
            }
        };
        if let Some(v) = units("ZENITH_SETTLEMENT_AUTO_APPROVE_MAX")? {
            p.auto_approve_max_gross = v;
        }
        if let Some(v) = units("ZENITH_SETTLEMENT_AUTO_APPROVE_MAX_WALLET")? {
            p.auto_approve_max_wallet_credit = v;
        }
        if let Ok(v) = std::env::var("ZENITH_SETTLEMENT_ALWAYS_REVIEW") {
            p.always_review = match v.trim() {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return Err("ZENITH_SETTLEMENT_ALWAYS_REVIEW must be true or false".into()),
            };
        }
        if let Ok(v) = std::env::var("ZENITH_SETTLEMENT_APPROVAL_TTL_SECS") {
            p.approval_ttl_secs = v
                .trim()
                .parse()
                .ok()
                .filter(|n: &i64| *n > 0)
                .ok_or("ZENITH_SETTLEMENT_APPROVAL_TTL_SECS must be a positive integer")?;
        }
        Ok(p)
    }

    /// Why this preview needs a human. Empty means auto-approve.
    pub fn review_reasons(&self, preview: &BatchPreview) -> Vec<String> {
        let t = &preview.totals;
        let mut reasons = Vec::new();
        if self.always_review {
            reasons.push("policy requires review of every batch".into());
        }
        if t.gross_value > self.auto_approve_max_gross {
            reasons.push(format!(
                "gross value {} exceeds auto-approve limit {}",
                t.gross_value, self.auto_approve_max_gross
            ));
        }
        if t.max_wallet_credit > self.auto_approve_max_wallet_credit {
            reasons.push(format!(
                "a wallet is credited {} (limit {})",
                t.max_wallet_credit, self.auto_approve_max_wallet_credit
            ));
        }
        if self.review_on_shortfall && t.shortfall > 0 {
            reasons.push(format!("collateral shortfall of {}", t.shortfall));
        }
        if self.review_on_non_twap_fixing && preview.fixing_source != FixingSource::Twap {
            reasons.push(format!(
                "fixing source is {}",
                preview.fixing_source.as_str()
            ));
        }
        reasons
    }
}

// ─── Queue ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ReviewError {
    Db(sqlx::Error),
    NotFound(i64),
    InvalidState {
        batch_id: i64,
        status: String,
        wanted: &'static str,
    },
    /// Four-eyes: the proposer cannot approve their own batch.
    SelfApproval,
    /// The reviewer approved a different preview than the one queued.
    DigestMismatch {
        expected: String,
        got: String,
    },
    /// Live data no longer matches the reviewed preview; re-propose.
    Stale {
        batch_id: i64,
    },
    Expired {
        batch_id: i64,
    },
    /// A batch for this expiry is executing or failed and needs an operator.
    InFlight {
        batch_id: i64,
    },
    ComplianceHold {
        batch_id: i64,
        wallets: usize,
    },
    Pins(PinError),
    Compliance(String),
    InvalidData(String),
    MissingNote,
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database error: {e}"),
            Self::NotFound(id) => write!(f, "settlement batch {id} not found"),
            Self::InvalidState {
                batch_id,
                status,
                wanted,
            } => {
                write!(f, "batch {batch_id} is {status}; expected {wanted}")
            }
            Self::SelfApproval => write!(f, "the proposer cannot approve their own batch"),
            Self::DigestMismatch { expected, got } => {
                write!(
                    f,
                    "reviewed preview {got} is not the queued preview {expected}"
                )
            }
            Self::Stale { batch_id } => {
                write!(
                    f,
                    "batch {batch_id} no longer matches live positions; re-propose it"
                )
            }
            Self::Expired { batch_id } => write!(f, "approval for batch {batch_id} has expired"),
            Self::InFlight { batch_id } => write!(
                f,
                "batch {batch_id} for this expiry needs operator attention"
            ),
            Self::ComplianceHold { batch_id, wallets } => {
                write!(
                    f,
                    "batch {batch_id} held: {wallets} wallet(s) failed compliance screening"
                )
            }
            Self::Pins(e) => write!(f, "settlement contract failed pin verification: {e}"),
            Self::Compliance(m) => write!(f, "compliance gate error: {m}"),
            Self::InvalidData(m) => write!(f, "invalid position data: {m}"),
            Self::MissingNote => write!(f, "a note is required"),
        }
    }
}

impl std::error::Error for ReviewError {}

impl From<sqlx::Error> for ReviewError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Batch {
    pub id: i64,
    pub status: String,
    pub preview: BatchPreview,
    pub review_reasons: Vec<String>,
    pub proposed_by: String,
    pub proposed_at: i64,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub approval_expires_at: Option<i64>,
    pub execution_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchEvent {
    pub event: String,
    pub actor: String,
    pub detail: String,
    pub created_at: i64,
}

async fn event(
    pool: &SqlitePool,
    batch_id: i64,
    event: &str,
    actor: &str,
    detail: &str,
    now: i64,
) -> Result<(), ReviewError> {
    sqlx::query(
        "INSERT INTO settlement_batch_events (batch_id, event, actor, detail, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(batch_id)
    .bind(event)
    .bind(actor)
    .bind(detail)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Moves a batch from one of `from` to `to`. Returns false if the batch was
/// not in an expected state (someone else got there first).
async fn transition(
    pool: &SqlitePool,
    batch_id: i64,
    from: &[&str],
    to: &str,
    now: i64,
) -> Result<bool, ReviewError> {
    let placeholders = vec!["?"; from.len()].join(", ");
    let sql = format!(
        "UPDATE settlement_batches SET status = ?, updated_at = ? WHERE id = ? AND status IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql).bind(to).bind(now).bind(batch_id);
    for s in from {
        q = q.bind(*s);
    }
    Ok(q.execute(pool).await?.rows_affected() == 1)
}

type BatchRow = (
    i64,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

pub async fn get(pool: &SqlitePool, batch_id: i64) -> Result<Batch, ReviewError> {
    let row: Option<BatchRow> = sqlx::query_as(
        "SELECT id, status, preview_json, review_reasons, proposed_by, proposed_at, decided_by, decision_note, \
                approval_expires_at, execution_ref \
         FROM settlement_batches WHERE id = ?",
    )
    .bind(batch_id)
    .fetch_optional(pool)
    .await?;
    let (
        id,
        status,
        preview_json,
        reasons,
        proposed_by,
        proposed_at,
        decided_by,
        decision_note,
        approval_expires_at,
        execution_ref,
    ) = row.ok_or(ReviewError::NotFound(batch_id))?;
    let preview: BatchPreview = serde_json::from_str(&preview_json)
        .map_err(|e| ReviewError::InvalidData(format!("stored preview for batch {id}: {e}")))?;
    Ok(Batch {
        id,
        status,
        preview,
        review_reasons: serde_json::from_str(&reasons).unwrap_or_default(),
        proposed_by,
        proposed_at,
        decided_by,
        decision_note,
        approval_expires_at,
        execution_ref,
    })
}

/// Batches waiting for a human, oldest first.
pub async fn pending(pool: &SqlitePool) -> Result<Vec<Batch>, ReviewError> {
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM settlement_batches WHERE status = 'pending_review' ORDER BY proposed_at, id")
            .fetch_all(pool)
            .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(get(pool, id).await?);
    }
    Ok(out)
}

pub async fn events(pool: &SqlitePool, batch_id: i64) -> Result<Vec<BatchEvent>, ReviewError> {
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT event, actor, detail, created_at FROM settlement_batch_events WHERE batch_id = ? ORDER BY id",
    )
    .bind(batch_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(event, actor, detail, created_at)| BatchEvent {
            event,
            actor,
            detail,
            created_at,
        })
        .collect())
}

/// Queues a batch. Re-proposing an identical preview returns the existing
/// batch (so keeper restarts are harmless). A different preview for the
/// same expiry supersedes a not-yet-executed one (it becomes `stale`).
pub async fn propose(
    pool: &SqlitePool,
    preview: &BatchPreview,
    policy: &ApprovalPolicy,
    proposer: &str,
    now: i64,
) -> Result<Batch, ReviewError> {
    if preview_digest(preview) != preview.sha256 {
        return Err(ReviewError::InvalidData(
            "preview hash does not match its contents".into(),
        ));
    }
    let live: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT id, status, preview_sha256 FROM settlement_batches \
         WHERE underlying = ? AND expires_at = ? \
           AND status IN ('pending_review', 'approved', 'auto_approved', 'executing', 'failed')",
    )
    .bind(&preview.underlying)
    .bind(&preview.expires_at)
    .fetch_optional(pool)
    .await?;
    if let Some((id, status, sha)) = live {
        if sha == preview.sha256 {
            return get(pool, id).await;
        }
        if matches!(status.as_str(), "executing" | "failed") {
            return Err(ReviewError::InFlight { batch_id: id });
        }
        if transition(
            pool,
            id,
            &["pending_review", "approved", "auto_approved"],
            "stale",
            now,
        )
        .await?
        {
            event(
                pool,
                id,
                "superseded",
                proposer,
                &format!("replaced by preview {}", preview.sha256),
                now,
            )
            .await?;
        }
    }

    let reasons = policy.review_reasons(preview);
    let (status, expires) = if reasons.is_empty() {
        ("auto_approved", Some(now + policy.approval_ttl_secs))
    } else {
        ("pending_review", None)
    };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO settlement_batches \
             (underlying, expires_at, fixing_price, fixing_source, preview_json, preview_sha256, gross_value, \
              positions, status, review_reasons, proposed_by, proposed_at, approval_expires_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&preview.underlying)
    .bind(&preview.expires_at)
    .bind(preview.fixing_price)
    .bind(preview.fixing_source.as_str())
    .bind(serde_json::to_string(preview).expect("preview serializes"))
    .bind(&preview.sha256)
    .bind(preview.totals.gross_value)
    .bind(preview.totals.positions)
    .bind(status)
    .bind(serde_json::to_string(&reasons).expect("reasons serialize"))
    .bind(proposer)
    .bind(now)
    .bind(expires)
    .bind(now)
    .fetch_one(pool)
    .await?;
    event(pool, id, "proposed", proposer, &preview.sha256, now).await?;
    if status == "auto_approved" {
        event(
            pool,
            id,
            "auto_approved",
            "policy",
            "within auto-approve limits",
            now,
        )
        .await?;
    } else {
        event(
            pool,
            id,
            "review_required",
            "policy",
            &reasons.join("; "),
            now,
        )
        .await?;
    }
    get(pool, id).await
}

/// Human approval. `reviewed_sha256` must be the hash shown to the
/// approver, so an approval cannot be applied to a different preview.
pub async fn approve(
    pool: &SqlitePool,
    batch_id: i64,
    approver: &str,
    reviewed_sha256: &str,
    note: &str,
    policy: &ApprovalPolicy,
    now: i64,
) -> Result<Batch, ReviewError> {
    let batch = get(pool, batch_id).await?;
    if batch.status != "pending_review" {
        return Err(ReviewError::InvalidState {
            batch_id,
            status: batch.status,
            wanted: "pending_review",
        });
    }
    if approver
        .trim()
        .eq_ignore_ascii_case(batch.proposed_by.trim())
    {
        return Err(ReviewError::SelfApproval);
    }
    if reviewed_sha256 != batch.preview.sha256 {
        return Err(ReviewError::DigestMismatch {
            expected: batch.preview.sha256,
            got: reviewed_sha256.into(),
        });
    }
    let done = sqlx::query(
        "UPDATE settlement_batches SET status = 'approved', decided_by = ?, decided_at = ?, decision_note = ?, \
             approval_expires_at = ?, updated_at = ? \
         WHERE id = ? AND status = 'pending_review'",
    )
    .bind(approver)
    .bind(now)
    .bind(note)
    .bind(now + policy.approval_ttl_secs)
    .bind(now)
    .bind(batch_id)
    .execute(pool)
    .await?;
    if done.rows_affected() != 1 {
        return Err(ReviewError::InvalidState {
            batch_id,
            status: get(pool, batch_id).await?.status,
            wanted: "pending_review",
        });
    }
    event(pool, batch_id, "approved", approver, note, now).await?;
    get(pool, batch_id).await
}

pub async fn reject(
    pool: &SqlitePool,
    batch_id: i64,
    operator: &str,
    note: &str,
    now: i64,
) -> Result<Batch, ReviewError> {
    if note.trim().is_empty() {
        return Err(ReviewError::MissingNote);
    }
    let done = sqlx::query(
        "UPDATE settlement_batches SET status = 'rejected', decided_by = ?, decided_at = ?, decision_note = ?, updated_at = ? \
         WHERE id = ? AND status IN ('pending_review', 'approved', 'auto_approved', 'failed')",
    )
    .bind(operator)
    .bind(now)
    .bind(note)
    .bind(now)
    .bind(batch_id)
    .execute(pool)
    .await?;
    if done.rows_affected() != 1 {
        let status = get(pool, batch_id).await?.status;
        return Err(ReviewError::InvalidState {
            batch_id,
            status,
            wanted: "pending_review, approved, auto_approved or failed",
        });
    }
    event(pool, batch_id, "rejected", operator, note, now).await?;
    get(pool, batch_id).await
}

/// Moves the funds for a batch. Implementations **must be idempotent on
/// `batch_id`**: a retry after a crash or a `failed` result must not settle
/// twice.
#[async_trait]
pub trait SettlementExecutor: Send + Sync {
    fn contract_id(&self) -> &str;
    async fn execute(&self, batch_id: i64, preview: &BatchPreview) -> Result<String, String>;
}

pub struct ExecuteContext<'a> {
    pub pool: &'a SqlitePool,
    pub executor: &'a dyn SettlementExecutor,
    pub compliance: &'a ComplianceGate,
    pub pins: &'a PinGuard,
    pub hashes: &'a dyn WasmHashSource,
}

/// Executes an approved (or auto-approved, or previously failed) batch,
/// only if the live preview still matches the reviewed one.
pub async fn execute(
    ctx: &ExecuteContext<'_>,
    batch_id: i64,
    actor: &str,
    now: i64,
) -> Result<Batch, ReviewError> {
    let pool = ctx.pool;
    let batch = get(pool, batch_id).await?;
    let executable = ["approved", "auto_approved", "failed"];
    if !executable.contains(&batch.status.as_str()) {
        return Err(ReviewError::InvalidState {
            batch_id,
            status: batch.status,
            wanted: "approved, auto_approved or failed",
        });
    }

    // 1. Approvals expire: a stale sign-off is not a sign-off.
    if batch.approval_expires_at.is_some_and(|t| now >= t) {
        if transition(pool, batch_id, &executable, "expired", now).await? {
            event(
                pool,
                batch_id,
                "expired",
                actor,
                "approval window elapsed",
                now,
            )
            .await?;
        }
        return Err(ReviewError::Expired { batch_id });
    }

    // 2. What executes must be exactly what was reviewed.
    let p = &batch.preview;
    let live = build_preview(
        pool,
        &p.underlying,
        &p.expires_at,
        p.fixing_price,
        p.fixing_source,
    )
    .await?;
    if live.sha256 != p.sha256 {
        if transition(pool, batch_id, &executable, "stale", now).await? {
            event(
                pool,
                batch_id,
                "stale",
                actor,
                &format!("live preview {} != reviewed {}", live.sha256, p.sha256),
                now,
            )
            .await?;
        }
        return Err(ReviewError::Stale { batch_id });
    }

    // 3. The settlement contract must still be the reviewed one.
    ctx.pins
        .ensure_live(ctx.executor.contract_id(), ctx.hashes)
        .await
        .map_err(ReviewError::Pins)?;

    // 4. Every party is screened. Settling against a blocked party's
    //    collateral is itself a dealing, so any hold stops the whole batch.
    let mut held = Vec::new();
    for w in &p.wallets {
        let d = match ctx.compliance.check(&w.wallet, Flow::Settlement, now).await {
            Ok(d) => d,
            Err(ComplianceError::Db(e)) => return Err(ReviewError::Db(e)),
            Err(e) => return Err(ReviewError::Compliance(e.to_string())),
        };
        if !d.allowed && d.enforced {
            held.push(format!("decision #{}", d.decision_id.unwrap_or_default()));
        }
    }
    if !held.is_empty() {
        if transition(pool, batch_id, &executable, "compliance_hold", now).await? {
            event(
                pool,
                batch_id,
                "compliance_hold",
                actor,
                &held.join(", "),
                now,
            )
            .await?;
        }
        return Err(ReviewError::ComplianceHold {
            batch_id,
            wallets: held.len(),
        });
    }

    // 5. Claim, so two executors cannot both run it.
    if !transition(pool, batch_id, &executable, "executing", now).await? {
        return Err(ReviewError::InFlight { batch_id });
    }
    event(pool, batch_id, "execution_started", actor, "", now).await?;

    match ctx.executor.execute(batch_id, p).await {
        Ok(reference) => {
            sqlx::query(
                "UPDATE settlement_batches SET status = 'executed', executed_by = ?, executed_at = ?, \
                     execution_ref = ?, updated_at = ? WHERE id = ? AND status = 'executing'",
            )
            .bind(actor)
            .bind(now)
            .bind(&reference)
            .bind(now)
            .bind(batch_id)
            .execute(pool)
            .await?;
            event(pool, batch_id, "executed", actor, &reference, now).await?;
        }
        Err(e) => {
            transition(pool, batch_id, &["executing"], "failed", now).await?;
            event(pool, batch_id, "failed", actor, &e, now).await?;
        }
    }
    get(pool, batch_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compliance::{ComplianceMode, CompliancePolicy, DenylistProvider};
    use crate::contract_pins::{sha256_hex, ContractPin, PinManifest, StaticHashSource};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const U: i64 = STROOPS_PER_UNIT;
    const EXPIRY: &str = "2026-10-30T08:00:00Z";
    const SETTLE_ID: &str = "CSETTLEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn leg(
        id: &str,
        wallet: &str,
        call: bool,
        short: bool,
        contracts: i64,
        strike: i64,
        collateral: i64,
    ) -> SettlementLeg {
        SettlementLeg {
            position_id: id.into(),
            wallet: wallet.into(),
            is_call: call,
            is_short: short,
            contracts: contracts * U,
            strike: strike * U,
            collateral: collateral * U,
        }
    }

    #[test]
    fn preview_math_is_exact() {
        // Fixing 900: the 1000 put is 100 ITM, the 950 call is OTM.
        let legs = vec![
            leg("p1", "GLONG", false, false, 2, 1000, 0), // long 2 puts: +200
            leg("p2", "GWRITER", false, true, 1, 1000, 1100), // short put: pays 100, gets 1000 back
            leg("p3", "GTHIN", false, true, 1, 1000, 60), // short put: 40 shortfall
            leg("p4", "GLONG", true, false, 1, 950, 0),   // OTM call: 0
        ];
        let p = compute_preview("XLM", EXPIRY, 900 * U, FixingSource::Twap, legs);
        let t = &p.totals;
        assert_eq!(t.long_payout, 200 * U);
        assert_eq!(t.short_paid, 160 * U);
        assert_eq!(t.shortfall, 40 * U);
        assert_eq!(
            t.platform_net,
            40 * U,
            "platform covers exactly the shortfall"
        );
        assert_eq!(t.collateral_unlocked, 1160 * U);
        assert_eq!(t.gross_value, 1360 * U);
        let writer = p.wallets.iter().find(|w| w.wallet == "GWRITER").unwrap();
        assert_eq!(writer.cash_delta, 1000 * U);
        let thin = p.wallets.iter().find(|w| w.wallet == "GTHIN").unwrap();
        assert_eq!((thin.cash_delta, thin.shortfall), (0, 40 * U));
        assert_eq!(t.max_wallet_credit, 1000 * U);
        // Fractional contracts are exact.
        let half = compute_preview(
            "XLM",
            EXPIRY,
            900 * U,
            FixingSource::Twap,
            vec![SettlementLeg {
                contracts: U / 2,
                ..leg("h", "G", false, false, 0, 1000, 0)
            }],
        );
        assert_eq!(half.totals.long_payout, 50 * U);
    }

    #[test]
    fn preview_hash_is_order_independent_and_content_sensitive() {
        let a = vec![
            leg("p1", "GA", true, false, 1, 10, 0),
            leg("p2", "GB", true, true, 1, 10, 20),
        ];
        let mut b = a.clone();
        b.reverse();
        let pa = compute_preview("XLM", EXPIRY, 15 * U, FixingSource::Twap, a.clone());
        assert_eq!(
            pa.sha256,
            compute_preview("XLM", EXPIRY, 15 * U, FixingSource::Twap, b).sha256
        );
        assert_ne!(
            pa.sha256,
            compute_preview("XLM", EXPIRY, 15 * U + 1, FixingSource::Twap, a.clone()).sha256
        );
        assert_ne!(
            pa.sha256,
            compute_preview("XLM", EXPIRY, 15 * U, FixingSource::Fallback, a).sha256
        );
    }

    #[test]
    fn policy_flags_what_needs_a_human() {
        let policy = ApprovalPolicy::default();
        let small = compute_preview(
            "XLM",
            EXPIRY,
            15 * U,
            FixingSource::Twap,
            vec![leg("p", "G", true, false, 1, 10, 0)],
        );
        assert!(policy.review_reasons(&small).is_empty());
        let fallback = compute_preview(
            "XLM",
            EXPIRY,
            15 * U,
            FixingSource::Fallback,
            vec![leg("p", "G", true, false, 1, 10, 0)],
        );
        assert_eq!(policy.review_reasons(&fallback).len(), 1);
        let big = compute_preview(
            "XLM",
            EXPIRY,
            20_000 * U,
            FixingSource::Twap,
            vec![leg("p", "G", true, false, 1, 10, 0)],
        );
        assert_eq!(
            policy.review_reasons(&big).len(),
            2,
            "gross and per-wallet limits"
        );
        let always = ApprovalPolicy {
            always_review: true,
            ..ApprovalPolicy::default()
        };
        assert_eq!(always.review_reasons(&small).len(), 1);
    }

    // ── Queue tests over a real schema ──

    struct Exec {
        calls: AtomicUsize,
        fail: Mutex<bool>,
    }

    #[async_trait]
    impl SettlementExecutor for Exec {
        fn contract_id(&self) -> &str {
            SETTLE_ID
        }
        async fn execute(&self, batch_id: i64, preview: &BatchPreview) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if *self.fail.lock().unwrap() {
                return Err("rpc timeout".into());
            }
            Ok(format!("tx-{batch_id}-{}", &preview.sha256[..8]))
        }
    }

    struct Env {
        pool: SqlitePool,
        exec: Exec,
        gate: ComplianceGate,
        pins: PinGuard,
        hashes: StaticHashSource,
        policy: ApprovalPolicy,
    }

    impl Env {
        async fn new() -> Self {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap();
            for sql in [
                include_str!("../migrations/0001_accounts.sql"),
                include_str!("../migrations/0002_positions.sql"),
                include_str!("../migrations/0007_position_expires_at.sql"),
                include_str!("../migrations/0012_compliance_screening.sql"),
                include_str!("../migrations/0014_settlement_review.sql"),
            ] {
                sqlx::raw_sql(sql).execute(&pool).await.unwrap();
            }
            sqlx::raw_sql(&format!(
                "INSERT INTO accounts (wallet_address) VALUES ('GLONG'), ('GWRITER'), ('GSANCTIONED');
                 INSERT INTO positions (id, wallet_address, underlying, strike, expiry_days, option_type,
                     position_type, contracts, entry_premium, entry_spot, collateral, expires_at) VALUES
                   ('p1', 'GLONG',   'XLM', 1000, 7, 'put', 'long',  2, 10, 1000, 0,    '{EXPIRY}'),
                   ('p2', 'GWRITER', 'XLM', 1000, 7, 'put', 'short', 2, 10, 1000, 2200, '{EXPIRY}'),
                   ('p3', 'GLONG',   'BTC', 1000, 7, 'put', 'long',  1, 10, 1000, 0,    '{EXPIRY}');"
            ))
            .execute(&pool)
            .await
            .unwrap();
            let gate = ComplianceGate::new(
                pool.clone(),
                Arc::new(DenylistProvider::new(
                    "t",
                    [("GSANCTIONED".to_string(), "SDN".to_string())],
                )),
                CompliancePolicy {
                    mode: ComplianceMode::Enforce,
                    ..CompliancePolicy::default()
                },
            );
            let mut hashes = StaticHashSource::default();
            hashes.0.insert(SETTLE_ID.into(), sha256_hex(b"settlement"));
            let manifest = PinManifest {
                schema_version: 1,
                contracts: vec![ContractPin {
                    name: "zenith-options".into(),
                    network: "testnet".into(),
                    contract_id: Some(SETTLE_ID.into()),
                    wasm_sha256: sha256_hex(b"settlement"),
                    source_repo: "r".into(),
                    source_commit: "b".repeat(40),
                    build_image: format!("img@sha256:{}", "0".repeat(64)),
                    audit_ref: None,
                    artifact: None,
                }],
            };
            let pins = PinGuard::verify(&manifest, "testnet", &hashes)
                .await
                .unwrap();
            Self {
                pool,
                exec: Exec {
                    calls: AtomicUsize::new(0),
                    fail: Mutex::new(false),
                },
                gate,
                pins,
                hashes,
                policy: ApprovalPolicy {
                    auto_approve_max_gross: 1_000 * U,
                    ..ApprovalPolicy::default()
                },
            }
        }

        fn ctx(&self) -> ExecuteContext<'_> {
            ExecuteContext {
                pool: &self.pool,
                executor: &self.exec,
                compliance: &self.gate,
                pins: &self.pins,
                hashes: &self.hashes,
            }
        }

        async fn preview(&self, fixing: i64) -> BatchPreview {
            build_preview(&self.pool, "XLM", EXPIRY, fixing * U, FixingSource::Twap)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn high_value_batch_waits_for_a_second_person() {
        let env = Env::new().await;
        let preview = env.preview(900).await;
        assert_eq!(preview.totals.positions, 2, "only the XLM expiry");
        assert_eq!(preview.totals.long_payout, 200 * U);

        let b = propose(&env.pool, &preview, &env.policy, "keeper", 100)
            .await
            .unwrap();
        assert_eq!(b.status, "pending_review");
        assert!(!b.review_reasons.is_empty());
        assert_eq!(pending(&env.pool).await.unwrap().len(), 1);
        // Not executable before approval.
        assert!(matches!(
            execute(&env.ctx(), b.id, "keeper", 101).await,
            Err(ReviewError::InvalidState { .. })
        ));

        // Four-eyes and digest checks.
        assert!(matches!(
            approve(
                &env.pool,
                b.id,
                "keeper",
                &preview.sha256,
                "",
                &env.policy,
                102
            )
            .await,
            Err(ReviewError::SelfApproval)
        ));
        assert!(matches!(
            approve(
                &env.pool,
                b.id,
                "alice",
                &"0".repeat(64),
                "",
                &env.policy,
                102
            )
            .await,
            Err(ReviewError::DigestMismatch { .. })
        ));
        let b = approve(
            &env.pool,
            b.id,
            "alice",
            &preview.sha256,
            "checked fixing vs. oracle",
            &env.policy,
            103,
        )
        .await
        .unwrap();
        assert_eq!(b.status, "approved");

        let b = execute(&env.ctx(), b.id, "keeper", 104).await.unwrap();
        assert_eq!(b.status, "executed");
        assert!(b
            .execution_ref
            .unwrap()
            .starts_with(&format!("tx-{}", b.id)));
        // Executed is terminal.
        assert!(execute(&env.ctx(), b.id, "keeper", 105).await.is_err());
        assert_eq!(env.exec.calls.load(Ordering::SeqCst), 1);

        let log: Vec<String> = events(&env.pool, b.id)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(
            log,
            [
                "proposed",
                "review_required",
                "approved",
                "execution_started",
                "executed"
            ]
        );
        assert!(sqlx::query("DELETE FROM settlement_batch_events")
            .execute(&env.pool)
            .await
            .is_err());
        assert!(
            sqlx::query("UPDATE settlement_batches SET preview_sha256 = 'x'")
                .execute(&env.pool)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn small_batches_auto_approve_and_reproposal_is_idempotent() {
        let env = Env::new().await;
        let policy = ApprovalPolicy::default();
        let preview = env.preview(995).await; // 10 + 2200 unlocked: under 10k
        let b = propose(&env.pool, &preview, &policy, "keeper", 1)
            .await
            .unwrap();
        assert_eq!(b.status, "auto_approved");
        let again = propose(&env.pool, &preview, &policy, "keeper", 2)
            .await
            .unwrap();
        assert_eq!(again.id, b.id);
        assert_eq!(
            execute(&env.ctx(), b.id, "keeper", 3).await.unwrap().status,
            "executed"
        );
    }

    #[tokio::test]
    async fn drift_after_review_makes_the_batch_stale() {
        let env = Env::new().await;
        let preview = env.preview(900).await;
        let b = propose(&env.pool, &preview, &env.policy, "keeper", 1)
            .await
            .unwrap();
        approve(
            &env.pool,
            b.id,
            "alice",
            &preview.sha256,
            "",
            &env.policy,
            2,
        )
        .await
        .unwrap();
        // A position changed after the human looked at the numbers.
        sqlx::query("UPDATE positions SET contracts = 3 WHERE id = 'p1'")
            .execute(&env.pool)
            .await
            .unwrap();
        assert!(matches!(
            execute(&env.ctx(), b.id, "keeper", 3).await,
            Err(ReviewError::Stale { .. })
        ));
        assert_eq!(get(&env.pool, b.id).await.unwrap().status, "stale");
        assert_eq!(env.exec.calls.load(Ordering::SeqCst), 0);
        // A fresh proposal can be made for the same expiry.
        let fresh = propose(&env.pool, &env.preview(900).await, &env.policy, "keeper", 4)
            .await
            .unwrap();
        assert_ne!(fresh.id, b.id);
    }

    #[tokio::test]
    async fn a_changed_proposal_supersedes_the_unexecuted_one() {
        let env = Env::new().await;
        let first = propose(&env.pool, &env.preview(900).await, &env.policy, "keeper", 1)
            .await
            .unwrap();
        let second = propose(&env.pool, &env.preview(800).await, &env.policy, "keeper", 2)
            .await
            .unwrap();
        assert_eq!(get(&env.pool, first.id).await.unwrap().status, "stale");
        assert_eq!(second.status, "pending_review");
        // Approving the superseded one is impossible.
        assert!(approve(
            &env.pool,
            first.id,
            "alice",
            &first.preview.sha256,
            "",
            &env.policy,
            3
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn approvals_expire() {
        let env = Env::new().await;
        let preview = env.preview(900).await;
        let b = propose(&env.pool, &preview, &env.policy, "keeper", 1)
            .await
            .unwrap();
        approve(
            &env.pool,
            b.id,
            "alice",
            &preview.sha256,
            "",
            &env.policy,
            10,
        )
        .await
        .unwrap();
        let late = 10 + env.policy.approval_ttl_secs;
        assert!(matches!(
            execute(&env.ctx(), b.id, "keeper", late).await,
            Err(ReviewError::Expired { .. })
        ));
        assert_eq!(get(&env.pool, b.id).await.unwrap().status, "expired");
    }

    #[tokio::test]
    async fn failed_execution_can_be_retried_but_not_double_proposed() {
        let env = Env::new().await;
        let preview = env.preview(900).await;
        let b = propose(&env.pool, &preview, &env.policy, "keeper", 1)
            .await
            .unwrap();
        approve(
            &env.pool,
            b.id,
            "alice",
            &preview.sha256,
            "",
            &env.policy,
            2,
        )
        .await
        .unwrap();
        *env.exec.fail.lock().unwrap() = true;
        assert_eq!(
            execute(&env.ctx(), b.id, "keeper", 3).await.unwrap().status,
            "failed"
        );
        // A different proposal cannot sneak in while this one is unresolved.
        assert!(matches!(
            propose(&env.pool, &env.preview(800).await, &env.policy, "keeper", 4).await,
            Err(ReviewError::InFlight { .. })
        ));
        *env.exec.fail.lock().unwrap() = false;
        assert_eq!(
            execute(&env.ctx(), b.id, "keeper", 5).await.unwrap().status,
            "executed"
        );
    }

    #[tokio::test]
    async fn sanctioned_party_holds_the_whole_batch() {
        let env = Env::new().await;
        sqlx::query("UPDATE positions SET wallet_address = 'GSANCTIONED' WHERE id = 'p2'")
            .execute(&env.pool)
            .await
            .unwrap();
        let preview = env.preview(900).await;
        let b = propose(&env.pool, &preview, &env.policy, "keeper", 1)
            .await
            .unwrap();
        approve(
            &env.pool,
            b.id,
            "alice",
            &preview.sha256,
            "",
            &env.policy,
            2,
        )
        .await
        .unwrap();
        assert!(matches!(
            execute(&env.ctx(), b.id, "keeper", 3).await,
            Err(ReviewError::ComplianceHold { wallets: 1, .. })
        ));
        assert_eq!(
            get(&env.pool, b.id).await.unwrap().status,
            "compliance_hold"
        );
        assert_eq!(env.exec.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn changed_settlement_contract_blocks_execution() {
        let mut env = Env::new().await;
        let preview = env.preview(995).await;
        let b = propose(&env.pool, &preview, &ApprovalPolicy::default(), "keeper", 1)
            .await
            .unwrap();
        env.hashes
            .0
            .insert(SETTLE_ID.into(), sha256_hex(b"upgraded"));
        assert!(matches!(
            execute(&env.ctx(), b.id, "keeper", 2).await,
            Err(ReviewError::Pins(_))
        ));
        assert_eq!(
            get(&env.pool, b.id).await.unwrap().status,
            "auto_approved",
            "still executable once fixed"
        );
        assert_eq!(env.exec.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reject_needs_a_note_and_is_final() {
        let env = Env::new().await;
        let b = propose(&env.pool, &env.preview(900).await, &env.policy, "keeper", 1)
            .await
            .unwrap();
        assert!(matches!(
            reject(&env.pool, b.id, "alice", " ", 2).await,
            Err(ReviewError::MissingNote)
        ));
        assert_eq!(
            reject(&env.pool, b.id, "alice", "fixing looks wrong", 2)
                .await
                .unwrap()
                .status,
            "rejected"
        );
        assert!(
            sqlx::query("UPDATE settlement_batches SET status = 'approved' WHERE id = ?")
                .bind(b.id)
                .execute(&env.pool)
                .await
                .is_err()
        );
    }
}
