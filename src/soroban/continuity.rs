//! Chain-state continuity: detect when the ledger line the backend built
//! off-chain state on is no longer canonical, and reconcile that state.
//!
//! Stellar's consensus gives finality, but what the backend *observes*
//! through RPC can still jump: an RPC fails over to a node that rolled back
//! an aborted ledger close, a load balancer alternates between nodes with
//! different histories, or a test network is reset to genesis. Each of these
//! means some off-chain state was derived from a transaction that is no
//! longer part of the canonical chain.
//!
//! [`ContinuityTracker`] keeps the canonical lineage (`ledger_lineage`), and
//! every observed ledger header is checked against it:
//!
//! | Observation | Verdict |
//! |---|---|
//! | `tip + 1`, parent hash == tip hash | extends |
//! | `tip + 1`, parent hash != tip hash | **aborted close** — tip was replaced |
//! | `== tip`, different hash | **hash mismatch** |
//! | `< tip`, hash matches lineage | stale view (lagging RPC), ignored |
//! | `< tip`, hash differs | **sequence regression** |
//! | passphrase changed, below the tracked window, or deeper than `max_reorg_depth` | **network reset** |
//!
//! On a discontinuity the tracker walks back with a [`LedgerSource`] to the
//! last common ancestor, and in one transaction discards the lineage above
//! it, records the event in `chain_discontinuities`, and marks every
//! `chain_derived_records` row at or above the fork point `reverted` (all of
//! them on a network reset). [`ContinuityTracker::reconcile_reverted`] then
//! locates each reverted record's transaction on the canonical chain: found
//! ones are re-derived through a [`Rederiver`] (`rederived`), missing ones
//! are handed to [`Rederiver::orphaned`] to undo (`orphaned`). Nothing derived
//! from a discarded ledger stays `active`.

use super::jsonrpc::{JsonRpc, RpcError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

const MIGRATION_SQL: &str = include_str!("../../migrations/0015_chain_continuity.sql");

pub async fn ensure_schema(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(MIGRATION_SQL).execute(db).await.map(|_| ())
}

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerHeader {
    pub sequence: u32,
    pub hash: String,
    /// Parent ledger hash, when the source provides it.
    pub prev_hash: Option<String>,
    pub close_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscontinuityKind {
    SequenceRegression,
    HashMismatch,
    AbortedClose,
    NetworkReset,
}

impl DiscontinuityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SequenceRegression => "sequence_regression",
            Self::HashMismatch => "hash_mismatch",
            Self::AbortedClose => "aborted_close",
            Self::NetworkReset => "network_reset",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discontinuity {
    pub id: String,
    pub kind: DiscontinuityKind,
    /// First sequence that is no longer canonical. Everything derived at or
    /// above it was reverted. `None` on a network reset (everything was).
    pub fork_sequence: Option<u32>,
    pub observed: LedgerHeader,
    /// Our lineage's hash at the observed sequence, if we had one.
    pub expected_hash: Option<String>,
    /// `(sequence, hash)` of the lineage entries that were discarded.
    pub discarded: Vec<(u32, String)>,
    pub epoch_before: i64,
    pub epoch_after: i64,
    pub reverted_records: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// First header seen (or first after a reset).
    Anchored,
    Extends,
    /// Ahead of the tip by more than one; parent continuity can't be checked
    /// until the caller backfills `from..to` (inclusive).
    Gap {
        from: u32,
        to: u32,
    },
    Duplicate,
    /// Older header that matches our lineage — a lagging RPC, not a fork.
    StaleView,
    Discontinuity(Discontinuity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Active,
    Reverted,
    Rederived,
    Orphaned,
}

impl RecordStatus {
    fn parse(s: &str) -> Self {
        match s {
            "reverted" => Self::Reverted,
            "rederived" => Self::Rederived,
            "orphaned" => Self::Orphaned,
            _ => Self::Active,
        }
    }
}

/// A piece of off-chain state derived from an on-chain transaction, e.g. a
/// settlement applied from an indexed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedRecord {
    pub id: String,
    /// What was derived (`settlement`, `position_settled`, ...).
    pub kind: String,
    /// The off-chain row it produced.
    pub ref_id: String,
    pub tx_hash: String,
    pub ledger_sequence: u32,
    pub ledger_hash: String,
    pub epoch: i64,
    pub status: RecordStatus,
    pub reverted_by: Option<String>,
    pub rederived_as: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDerivation {
    pub kind: String,
    pub ref_id: String,
    pub tx_hash: String,
    pub ledger_sequence: u32,
    pub ledger_hash: String,
}

/// Where a transaction landed on the canonical chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxLocation {
    pub ledger_sequence: u32,
    pub ledger_hash: String,
}

#[derive(Debug)]
pub enum ContinuityError {
    Db(sqlx::Error),
    Rpc(RpcError),
    /// A derivation referenced a ledger hash that isn't canonical.
    NonCanonicalLedger {
        sequence: u32,
        hash: String,
        canonical: String,
    },
    Rederive(String),
}

impl std::fmt::Display for ContinuityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "{e}"),
            Self::Rpc(e) => write!(f, "{e}"),
            Self::NonCanonicalLedger {
                sequence,
                hash,
                canonical,
            } => write!(
                f,
                "ledger {sequence} hash {hash} is not canonical (canonical: {canonical})"
            ),
            Self::Rederive(e) => write!(f, "re-derivation failed: {e}"),
        }
    }
}

impl std::error::Error for ContinuityError {}

impl From<sqlx::Error> for ContinuityError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl From<RpcError> for ContinuityError {
    fn from(e: RpcError) -> Self {
        Self::Rpc(e)
    }
}

// ─── Sources ─────────────────────────────────────────────────────────────────

/// Canonical ledger headers by sequence. `Ok(None)` means the source no
/// longer (or doesn't yet) have that ledger.
#[async_trait]
pub trait LedgerSource: Send + Sync {
    async fn ledger(&self, sequence: u32) -> Result<Option<LedgerHeader>, RpcError>;
}

/// Finds a transaction on the canonical chain.
#[async_trait]
pub trait TxLocator: Send + Sync {
    async fn locate(&self, tx_hash: &str) -> Result<Option<TxLocation>, RpcError>;
}

/// Re-applies (or undoes) off-chain effects for a reverted record.
#[async_trait]
pub trait Rederiver: Send + Sync {
    /// The transaction is on the canonical chain at `at`; re-derive the
    /// off-chain state from it and describe the new derivation.
    async fn rederive(&self, old: &DerivedRecord, at: &TxLocation)
        -> Result<NewDerivation, String>;
    /// The transaction is not on the canonical chain; undo its effects.
    async fn orphaned(&self, old: &DerivedRecord) -> Result<(), String>;
}

fn parse_rpc_ledger(v: &Value) -> Option<LedgerHeader> {
    let prev = v
        .get("headerJson")
        .and_then(|h| h.get("header").unwrap_or(h).get("previous_ledger_hash"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(LedgerHeader {
        sequence: v.get("sequence")?.as_u64()? as u32,
        hash: v.get("hash").or_else(|| v.get("id"))?.as_str()?.to_string(),
        prev_hash: prev,
        close_time: v
            .get("ledgerCloseTime")
            .and_then(|t| t.as_u64().or_else(|| t.as_str()?.parse().ok()))
            .unwrap_or(0),
    })
}

/// `LedgerSource` + `TxLocator` over Soroban RPC (`getLedgers`,
/// `getTransaction`).
pub struct RpcChainSource {
    rpc: Arc<dyn JsonRpc>,
}

impl RpcChainSource {
    pub fn new(rpc: Arc<dyn JsonRpc>) -> Self {
        Self { rpc }
    }

    pub async fn latest(&self) -> Result<LedgerHeader, RpcError> {
        let latest = self.rpc.call("getLatestLedger", json!({})).await?;
        let seq = latest
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::Malformed("getLatestLedger: missing sequence".into()))?;
        self.ledger(seq as u32)
            .await?
            .ok_or_else(|| RpcError::Malformed(format!("latest ledger {seq} not served")))
    }
}

#[async_trait]
impl LedgerSource for RpcChainSource {
    async fn ledger(&self, sequence: u32) -> Result<Option<LedgerHeader>, RpcError> {
        let res = self
            .rpc
            .call(
                "getLedgers",
                json!({ "startLedger": sequence, "pagination": { "limit": 1 }, "xdrFormat": "json" }),
            )
            .await;
        match res {
            Ok(v) => Ok(v
                .get("ledgers")
                .and_then(Value::as_array)
                .and_then(|l| l.first())
                .and_then(parse_rpc_ledger)
                .filter(|h| h.sequence == sequence)),
            // Outside the RPC's retention window.
            Err(RpcError::Node { code: -32602, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl TxLocator for RpcChainSource {
    async fn locate(&self, tx_hash: &str) -> Result<Option<TxLocation>, RpcError> {
        let v = self
            .rpc
            .call("getTransaction", json!({ "hash": tx_hash }))
            .await?;
        if v.get("status").and_then(Value::as_str) == Some("NOT_FOUND") {
            return Ok(None);
        }
        let Some(seq) = v.get("ledger").and_then(Value::as_u64).map(|s| s as u32) else {
            return Ok(None);
        };
        Ok(self.ledger(seq).await?.map(|h| TxLocation {
            ledger_sequence: seq,
            ledger_hash: h.hash,
        }))
    }
}

// ─── Tracker ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ContinuityConfig {
    pub passphrase: String,
    /// A fork deeper than this is treated as a network reset.
    pub max_reorg_depth: u32,
}

pub struct ContinuityTracker {
    db: SqlitePool,
    cfg: ContinuityConfig,
}

type LineageRow = (i64, String);
type RecordRow = (
    String,
    String,
    String,
    String,
    i64,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
);

fn record_from_row(r: RecordRow) -> DerivedRecord {
    DerivedRecord {
        id: r.0,
        kind: r.1,
        ref_id: r.2,
        tx_hash: r.3,
        ledger_sequence: r.4 as u32,
        ledger_hash: r.5,
        epoch: r.6,
        status: RecordStatus::parse(&r.7),
        reverted_by: r.8,
        rederived_as: r.9,
    }
}

const RECORD_COLUMNS: &str = "id, kind, ref_id, tx_hash, ledger_sequence, ledger_hash, epoch, \
                              status, reverted_by, rederived_as";

impl ContinuityTracker {
    pub fn new(db: SqlitePool, cfg: ContinuityConfig) -> Self {
        Self { db, cfg }
    }

    async fn state(&self) -> Result<(i64, Option<String>), sqlx::Error> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT epoch, passphrase FROM chain_continuity_state WHERE id = 1")
                .fetch_optional(&self.db)
                .await?;
        Ok(row.map(|(e, p)| (e, Some(p))).unwrap_or((0, None)))
    }

    pub async fn epoch(&self) -> Result<i64, sqlx::Error> {
        Ok(self.state().await?.0)
    }

    pub async fn tip(&self) -> Result<Option<(u32, String)>, sqlx::Error> {
        let row: Option<LineageRow> = sqlx::query_as(
            "SELECT sequence, hash FROM ledger_lineage ORDER BY sequence DESC LIMIT 1",
        )
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|(s, h)| (s as u32, h)))
    }

    async fn first_tracked(&self) -> Result<Option<u32>, sqlx::Error> {
        let (min,): (Option<i64>,) = sqlx::query_as("SELECT MIN(sequence) FROM ledger_lineage")
            .fetch_one(&self.db)
            .await?;
        Ok(min.map(|m| m as u32))
    }

    pub async fn canonical_hash(&self, sequence: u32) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT hash FROM ledger_lineage WHERE sequence = ?1")
                .bind(sequence as i64)
                .fetch_optional(&self.db)
                .await?;
        Ok(row.map(|(h,)| h))
    }

    async fn append(&self, h: &LedgerHeader, epoch: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO ledger_lineage (sequence, hash, prev_hash, close_time, epoch)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (sequence) DO NOTHING",
        )
        .bind(h.sequence as i64)
        .bind(&h.hash)
        .bind(&h.prev_hash)
        .bind(h.close_time as i64)
        .bind(epoch)
        .execute(&self.db)
        .await
        .map(|_| ())
    }

    async fn save_state(&self, epoch: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO chain_continuity_state (id, epoch, passphrase) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET epoch = excluded.epoch, passphrase = excluded.passphrase",
        )
        .bind(epoch)
        .bind(&self.cfg.passphrase)
        .execute(&self.db)
        .await
        .map(|_| ())
    }

    /// Walks back from `candidate` (first suspected-bad sequence) to the
    /// last ledger our lineage shares with the source. Returns the fork
    /// sequence, or `None` if no common ancestor was found within
    /// `max_reorg_depth` / the tracked window (→ network reset).
    async fn find_fork(
        &self,
        candidate: u32,
        source: &dyn LedgerSource,
    ) -> Result<Option<u32>, ContinuityError> {
        let Some(first) = self.first_tracked().await? else {
            return Ok(None);
        };
        let mut seq = candidate;
        while seq > first {
            if candidate - seq >= self.cfg.max_reorg_depth {
                return Ok(None);
            }
            let parent = seq - 1;
            let ours = self.canonical_hash(parent).await?;
            match (ours, source.ledger(parent).await?) {
                (Some(ours), Some(theirs)) if ours == theirs.hash => return Ok(Some(seq)),
                (None, _) => return Ok(Some(seq)), // hole in our lineage: can't go further
                // Source no longer serves it: conservatively fork here.
                (Some(_), None) => return Ok(Some(seq)),
                _ => seq = parent,
            }
        }
        Ok(None)
    }

    /// Checks `header` against the lineage, recording and reconciling any
    /// discontinuity it reveals. `source` is used to locate the fork point.
    pub async fn observe(
        &self,
        header: &LedgerHeader,
        source: &dyn LedgerSource,
    ) -> Result<Verdict, ContinuityError> {
        let (epoch, passphrase) = self.state().await?;
        let tip = self.tip().await?;

        if passphrase
            .as_deref()
            .is_some_and(|p| p != self.cfg.passphrase)
        {
            let d = self
                .apply(DiscontinuityKind::NetworkReset, None, header, None, epoch)
                .await?;
            return Ok(Verdict::Discontinuity(d));
        }

        let Some((tip_seq, tip_hash)) = tip else {
            self.save_state(epoch).await?;
            self.append(header, epoch).await?;
            return Ok(Verdict::Anchored);
        };

        let s = header.sequence;
        let (kind, candidate, expected) = if s == tip_seq + 1 {
            match &header.prev_hash {
                Some(prev) if *prev != tip_hash => (
                    DiscontinuityKind::AbortedClose,
                    tip_seq,
                    Some(tip_hash.clone()),
                ),
                _ => {
                    self.append(header, epoch).await?;
                    return Ok(Verdict::Extends);
                }
            }
        } else if s > tip_seq + 1 {
            return Ok(Verdict::Gap {
                from: tip_seq + 1,
                to: s - 1,
            });
        } else if s == tip_seq {
            if header.hash == tip_hash {
                return Ok(Verdict::Duplicate);
            }
            (DiscontinuityKind::HashMismatch, s, Some(tip_hash))
        } else {
            match self.canonical_hash(s).await? {
                Some(h) if h == header.hash => return Ok(Verdict::StaleView),
                Some(h) => (DiscontinuityKind::SequenceRegression, s, Some(h)),
                // Below everything we track: the chain restarted.
                None => {
                    let d = self
                        .apply(DiscontinuityKind::NetworkReset, None, header, None, epoch)
                        .await?;
                    return Ok(Verdict::Discontinuity(d));
                }
            }
        };

        let d = match self.find_fork(candidate, source).await? {
            Some(fork) => {
                self.apply(kind, Some(fork), header, expected, epoch)
                    .await?
            }
            None => {
                self.apply(
                    DiscontinuityKind::NetworkReset,
                    None,
                    header,
                    expected,
                    epoch,
                )
                .await?
            }
        };
        Ok(Verdict::Discontinuity(d))
    }

    /// Atomically discards lineage at/above the fork (all of it on a reset),
    /// reverts derived records built on it, logs the discontinuity and
    /// re-anchors on `header` when it is now the tip.
    async fn apply(
        &self,
        kind: DiscontinuityKind,
        fork: Option<u32>,
        header: &LedgerHeader,
        expected_hash: Option<String>,
        epoch: i64,
    ) -> Result<Discontinuity, ContinuityError> {
        let id = uuid::Uuid::new_v4().to_string();
        let floor = fork.map(i64::from).unwrap_or(i64::MIN);
        let epoch_after = if fork.is_none() { epoch + 1 } else { epoch };
        let reason = format!("{} at ledger {}", kind.as_str(), header.sequence);

        let mut tx = self.db.begin().await?;
        let discarded: Vec<(u32, String)> = sqlx::query_as::<_, LineageRow>(
            "SELECT sequence, hash FROM ledger_lineage WHERE sequence >= ?1 ORDER BY sequence",
        )
        .bind(floor)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|(s, h)| (s as u32, h))
        .collect();
        sqlx::query("DELETE FROM ledger_lineage WHERE sequence >= ?1")
            .bind(floor)
            .execute(&mut *tx)
            .await?;
        // Logged first: reverted records reference it.
        sqlx::query(
            "INSERT INTO chain_discontinuities
                (id, kind, fork_sequence, observed_sequence, observed_hash, expected_hash,
                 discarded, epoch_before, epoch_after, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, strftime('%s', 'now'))",
        )
        .bind(&id)
        .bind(kind.as_str())
        .bind(fork.map(i64::from))
        .bind(header.sequence as i64)
        .bind(&header.hash)
        .bind(&expected_hash)
        .bind(serde_json::to_string(&discarded).unwrap_or_default())
        .bind(epoch)
        .bind(epoch_after)
        .execute(&mut *tx)
        .await?;
        // On a reset every active record is from the old chain; otherwise
        // only those at or above the fork point.
        let reverted = sqlx::query(
            "UPDATE chain_derived_records
             SET status = 'reverted', reverted_by = ?1, revert_reason = ?2,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE status = 'active' AND ledger_sequence >= ?3",
        )
        .bind(&id)
        .bind(&reason)
        .bind(floor)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query("UPDATE chain_discontinuities SET reverted_records = ?2 WHERE id = ?1")
            .bind(&id)
            .bind(reverted as i64)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO chain_continuity_state (id, epoch, passphrase) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET epoch = excluded.epoch, passphrase = excluded.passphrase",
        )
        .bind(epoch_after)
        .bind(&self.cfg.passphrase)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // Re-anchor on the observed header if it now sits on top of the
        // surviving lineage (or there is none left).
        let tip = self.tip().await?;
        if tip.as_ref().is_none_or(|(s, _)| header.sequence == s + 1) {
            self.append(header, epoch_after).await?;
        }

        tracing::error!(
            target: "zenith::continuity",
            discontinuity_id = %id,
            kind = kind.as_str(),
            fork_sequence = ?fork,
            observed_sequence = header.sequence,
            reverted_records = reverted,
            "chain discontinuity detected; derived state reverted"
        );

        Ok(Discontinuity {
            id,
            kind,
            fork_sequence: fork,
            observed: header.clone(),
            expected_hash,
            discarded,
            epoch_before: epoch,
            epoch_after,
            reverted_records: reverted,
        })
    }

    /// Registers off-chain state derived from a transaction. The ledger hash
    /// must match the lineage when we have that ledger, so nothing can be
    /// recorded against an already-discarded line.
    pub async fn record_derivation(&self, d: &NewDerivation) -> Result<String, ContinuityError> {
        if let Some(canonical) = self.canonical_hash(d.ledger_sequence).await? {
            if canonical != d.ledger_hash {
                return Err(ContinuityError::NonCanonicalLedger {
                    sequence: d.ledger_sequence,
                    hash: d.ledger_hash.clone(),
                    canonical,
                });
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO chain_derived_records
                (id, kind, ref_id, tx_hash, ledger_sequence, ledger_hash, epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&id)
        .bind(&d.kind)
        .bind(&d.ref_id)
        .bind(&d.tx_hash)
        .bind(d.ledger_sequence as i64)
        .bind(&d.ledger_hash)
        .bind(self.epoch().await?)
        .execute(&self.db)
        .await?;
        Ok(id)
    }

    pub async fn record(&self, id: &str) -> Result<Option<DerivedRecord>, sqlx::Error> {
        let row: Option<RecordRow> = sqlx::query_as(&format!(
            "SELECT {RECORD_COLUMNS} FROM chain_derived_records WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(record_from_row))
    }

    pub async fn records_with_status(
        &self,
        status: RecordStatus,
    ) -> Result<Vec<DerivedRecord>, sqlx::Error> {
        let s = serde_json::to_value(status).unwrap();
        let rows: Vec<RecordRow> = sqlx::query_as(&format!(
            "SELECT {RECORD_COLUMNS} FROM chain_derived_records WHERE status = ?1
             ORDER BY ledger_sequence, id"
        ))
        .bind(s.as_str().unwrap())
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    /// Resolves up to `limit` reverted records: re-derive from the canonical
    /// chain where the transaction still exists, otherwise orphan. Records
    /// whose transaction can't be located yet (RPC error, or its ledger
    /// isn't in our lineage yet) stay `reverted` for the next pass.
    pub async fn reconcile_reverted(
        &self,
        locator: &dyn TxLocator,
        rederiver: &dyn Rederiver,
        limit: usize,
    ) -> Result<ReconcileSummary, ContinuityError> {
        let mut summary = ReconcileSummary::default();
        for old in self
            .records_with_status(RecordStatus::Reverted)
            .await?
            .into_iter()
            .take(limit)
        {
            let located = match locator.locate(&old.tx_hash).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(target: "zenith::continuity", tx_hash = %old.tx_hash, error = %e, "locate failed");
                    summary.pending += 1;
                    continue;
                }
            };
            match located {
                Some(at) => {
                    match self.canonical_hash(at.ledger_sequence).await? {
                        Some(h) if h == at.ledger_hash => {}
                        // Canonical lineage hasn't reached it (or disagrees): wait.
                        _ => {
                            summary.pending += 1;
                            continue;
                        }
                    }
                    let new = rederiver
                        .rederive(&old, &at)
                        .await
                        .map_err(ContinuityError::Rederive)?;
                    let new_id = self.record_derivation(&new).await?;
                    sqlx::query(
                        "UPDATE chain_derived_records
                         SET status = 'rederived', rederived_as = ?2,
                             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                         WHERE id = ?1 AND status = 'reverted'",
                    )
                    .bind(&old.id)
                    .bind(&new_id)
                    .execute(&self.db)
                    .await?;
                    summary.rederived += 1;
                }
                None => {
                    rederiver
                        .orphaned(&old)
                        .await
                        .map_err(ContinuityError::Rederive)?;
                    sqlx::query(
                        "UPDATE chain_derived_records
                         SET status = 'orphaned', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                         WHERE id = ?1 AND status = 'reverted'",
                    )
                    .bind(&old.id)
                    .execute(&self.db)
                    .await?;
                    summary.orphaned += 1;
                }
            }
        }
        Ok(summary)
    }

    /// Every discontinuity detected, oldest first — the operator audit trail.
    pub async fn discontinuities(&self) -> Result<Vec<DiscontinuitySummary>, sqlx::Error> {
        let rows: Vec<(String, String, Option<i64>, i64, i64)> = sqlx::query_as(
            "SELECT id, kind, fork_sequence, observed_sequence, reverted_records
             FROM chain_discontinuities ORDER BY detected_at, rowid",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, kind, fork, observed, reverted)| DiscontinuitySummary {
                    id,
                    kind,
                    fork_sequence: fork.map(|f| f as u32),
                    observed_sequence: observed as u32,
                    reverted_records: reverted as u64,
                },
            )
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscontinuitySummary {
    pub id: String,
    pub kind: String,
    pub fork_sequence: Option<u32>,
    pub observed_sequence: u32,
    pub reverted_records: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReconcileSummary {
    pub rederived: u64,
    pub orphaned: u64,
    pub pending: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_ledgers_entry() {
        let h = parse_rpc_ledger(&json!({
            "hash": "aa", "sequence": 10, "ledgerCloseTime": "1700000000",
            "headerJson": {"hash": "aa", "header": {"previous_ledger_hash": "99"}}
        }))
        .unwrap();
        assert_eq!(
            h,
            LedgerHeader {
                sequence: 10,
                hash: "aa".into(),
                prev_hash: Some("99".into()),
                close_time: 1_700_000_000
            }
        );
    }
}
