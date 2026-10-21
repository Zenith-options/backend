//! Event indexer: coverage tracking, gap detection and the shared projection
//! step.
//!
//! The indexer follows the chain tip, but a restart, an RPC outage or a
//! cursor regression can leave holes in the ledgers it has processed. The
//! ranges it has covered are therefore recorded explicitly, so a gap is
//! *detectable* instead of silently corrupting the off-chain state.
//!
//! The projection step is a pure function over a decoded event batch. The
//! live tip-following loop and the historical backfill both call it, so a
//! replayed range produces exactly the same rows as the original pass — and
//! because every row is keyed by transaction hash plus event index, writing
//! it twice is a no-op.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::chain::bindings::events::DecodedEvent;
use crate::chain::rpc::{RpcError, SorobanRpc};
use crate::chain::types::{EventResult, GetEventsParams, GetEventsResponse};

/// Default name of the indexed stream.
pub const DEFAULT_STREAM: &str = "zenith";

/// Default number of ledgers fetched per event page.
pub const DEFAULT_PAGE_LIMIT: u32 = 200;

/// How often the tip-following loop polls the node.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// A half-open range of ledgers, `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LedgerRange {
    /// First ledger of the range (inclusive).
    pub from: u32,
    /// Last ledger of the range (exclusive).
    pub to: u32,
}

impl LedgerRange {
    /// Create a range, normalising an empty range away.
    pub fn new(from: u32, to: u32) -> Self {
        Self { from, to }
    }

    /// Number of ledgers covered by the range.
    pub fn len(&self) -> u32 {
        self.to.saturating_sub(self.from)
    }

    /// Whether the range covers no ledger.
    pub fn is_empty(&self) -> bool {
        self.to <= self.from
    }

    /// Whether the range covers `ledger`.
    pub fn contains(&self, ledger: u32) -> bool {
        ledger >= self.from && ledger < self.to
    }
}

/// A missing range of ledgers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerGap {
    /// First missing ledger (inclusive).
    pub from: u32,
    /// Last missing ledger (exclusive).
    pub to: u32,
}

impl LedgerGap {
    /// The gap as a range.
    pub fn range(&self) -> LedgerRange {
        LedgerRange::new(self.from, self.to)
    }

    /// Number of ledgers missing.
    pub fn len(&self) -> u32 {
        self.to.saturating_sub(self.from)
    }

    /// Whether the gap is empty.
    pub fn is_empty(&self) -> bool {
        self.to <= self.from
    }
}

/// Where a covered range came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeSource {
    /// Written by the live tip-following loop.
    Live,
    /// Written by a historical backfill.
    Backfill,
}

impl RangeSource {
    /// The value stored in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Backfill => "backfill",
        }
    }
}

/// Merge overlapping and adjacent ranges into a canonical, sorted list.
///
/// Two ranges that touch (`[0, 10)` and `[10, 20)`) are merged as well: the
/// coverage is contiguous, so there is no gap between them.
pub fn merge_ranges(ranges: &[LedgerRange]) -> Vec<LedgerRange> {
    let mut sorted: Vec<LedgerRange> = ranges.iter().copied().filter(|r| !r.is_empty()).collect();
    sorted.sort_by_key(|range| (range.from, range.to));

    let mut merged: Vec<LedgerRange> = Vec::with_capacity(sorted.len());
    for range in sorted {
        match merged.last_mut() {
            Some(last) if range.from <= last.to => {
                last.to = last.to.max(range.to);
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// Find the ledgers that are missing between the covered ranges.
///
/// `tip` is the newest closed ledger; everything up to and including it is
/// expected to be covered. The coverage starts at the oldest range that has
/// been indexed: the ledgers before it were never part of the stream, so
/// they are not a gap.
pub fn detect_gaps(ranges: &[LedgerRange], tip: u32) -> Vec<LedgerGap> {
    let merged = merge_ranges(ranges);
    let mut gaps = Vec::new();
    // Nothing indexed at all: everything up to the tip is missing.
    let mut expected = merged.first().map(|range| range.from).unwrap_or(0);

    for range in merged {
        if range.from > expected {
            gaps.push(LedgerGap {
                from: expected,
                to: range.from,
            });
        }
        expected = expected.max(range.to);
        if expected > tip {
            return gaps;
        }
    }

    if expected <= tip {
        gaps.push(LedgerGap {
            from: expected,
            to: tip.saturating_add(1),
        });
    }
    gaps
}

/// Find the parts of ranges that are covered more than once.
///
/// Overlapping coverage is not an error (a replay writes the same rows
/// again) but it is worth reporting: it usually means two workers indexed the
/// same ledgers.
pub fn detect_overlaps(ranges: &[LedgerRange]) -> Vec<LedgerRange> {
    let mut sorted: Vec<LedgerRange> = ranges.iter().copied().filter(|r| !r.is_empty()).collect();
    sorted.sort_by_key(|range| (range.from, range.to));

    let mut overlaps = Vec::new();
    let mut covered_to: Option<u32> = None;
    for range in sorted {
        let overlap = match covered_to {
            Some(covered) if range.from < covered => {
                Some(LedgerRange::new(range.from, covered.min(range.to)))
            }
            _ => None,
        };
        if let Some(overlap) = overlap {
            overlaps.push(overlap);
        }
        covered_to = Some(covered_to.map_or(range.to, |covered| covered.max(range.to)));
    }
    overlaps
}

/// The identity of an event: the transaction hash plus the index of the event
/// inside that transaction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventKey {
    /// Hash of the transaction that emitted the event.
    pub tx_hash: String,
    /// Index of the event inside the transaction.
    pub event_index: i32,
}

/// A decoded chain event, ready to be projected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedChainEvent {
    /// The ledger the event was emitted in.
    pub ledger: u32,
    /// The paging token the node gave the event; stable across replays.
    pub paging_token: String,
    /// The transaction that emitted the event.
    pub tx_hash: String,
    /// The index of the event inside the transaction.
    pub event_index: i32,
    /// The contract that emitted the event.
    pub contract_id: String,
    /// The type of the event, matching a contract binding variant.
    pub event_type: String,
    /// The raw payload of the event.
    pub payload: Value,
}

impl DecodedChainEvent {
    /// The typed event, when the contract bindings know the type.
    pub fn typed(&self) -> Option<DecodedEvent> {
        let mut object = self.payload.as_object()?.clone();
        object.insert("type".to_string(), Value::String(self.event_type.clone()));
        serde_json::from_value(Value::Object(object)).ok()
    }
}

/// A row produced by the projection step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionRow {
    /// The logical projection the event belongs to.
    pub projection: String,
    /// The identity of the event the row was derived from.
    pub event: EventKey,
    /// The ledger the event was emitted in.
    pub ledger: u32,
    /// The projected values.
    pub payload: Value,
}

/// The projection step: a pure function over a decoded event batch.
///
/// Unknown event types produce no row: they are still stored raw, but they
/// never break the projection of the events around them.
pub fn project_events(events: &[DecodedChainEvent]) -> Vec<ProjectionRow> {
    events.iter().filter_map(project_event).collect()
}

/// The projection of a single event.
pub fn project_event(event: &DecodedChainEvent) -> Option<ProjectionRow> {
    let typed = event.typed()?;
    let (projection, payload) = match typed {
        DecodedEvent::OptionMinted {
            series_id,
            strike,
            expiry,
            is_call,
            contracts,
            writer,
        } => (
            "option_minted",
            serde_json::json!({
                "series_id": series_id,
                "strike": strike,
                "expiry": expiry,
                "is_call": is_call,
                "contracts": contracts,
                "writer": writer,
            }),
        ),
        DecodedEvent::OptionExercised {
            series_id,
            buyer,
            payout,
        } => (
            "option_exercised",
            serde_json::json!({
                "series_id": series_id,
                "buyer": buyer,
                "payout": payout,
            }),
        ),
        DecodedEvent::CollateralDeposited { wallet, amount } => (
            "collateral_deposited",
            serde_json::json!({
                "wallet": wallet,
                "amount": amount,
            }),
        ),
        DecodedEvent::ContractUpgraded {
            new_wasm_hash,
            new_version,
        } => (
            "contract_upgraded",
            serde_json::json!({
                "new_wasm_hash": new_wasm_hash,
                "new_version": new_version,
            }),
        ),
    };

    Some(ProjectionRow {
        projection: projection.to_string(),
        event: EventKey {
            tx_hash: event.tx_hash.clone(),
            event_index: event.event_index,
        },
        ledger: event.ledger,
        payload,
    })
}

/// Remove duplicated rows, keeping the first occurrence of every event.
///
/// The result is the same no matter how often the same batch is replayed.
pub fn dedupe_projections(rows: Vec<ProjectionRow>) -> Vec<ProjectionRow> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|row| seen.insert(row.event.clone()))
        .collect()
}

/// Decode a page of RPC events into decoded chain events.
///
/// The event index is the index of the event *inside its transaction*, which
/// is what identifies an event: the same event always gets the same index, no
/// matter which page it was delivered in. Events without a transaction hash
/// fall back to their paging token, which the node keeps stable.
///
/// The payload is the *decoded* event data. A node returns it base64 encoded
/// as XDR; the contract binding decoder turns it into the same shape, so the
/// projection step only ever sees the decoded values.
pub fn decode_events(events: &[EventResult]) -> Vec<DecodedChainEvent> {
    let mut per_transaction: std::collections::HashMap<String, i32> =
        std::collections::HashMap::new();

    events
        .iter()
        .map(|event| {
            let tx_hash = event
                .tx_hash
                .clone()
                .filter(|hash| !hash.is_empty())
                .unwrap_or_else(|| event.id.clone());
            let event_index = {
                let counter = per_transaction.entry(tx_hash.clone()).or_insert(0);
                let index = *counter;
                *counter += 1;
                index
            };
            let mut payload = serde_json::from_str::<Value>(&event.value)
                .ok()
                .filter(|value| value.is_object())
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            if let Some(object) = payload.as_object_mut() {
                object.entry("topics").or_insert_with(|| {
                    Value::Array(
                        event
                            .topic
                            .iter()
                            .map(|topic| Value::String(topic.clone()))
                            .collect(),
                    )
                });
            }
            DecodedChainEvent {
                ledger: event.ledger,
                paging_token: event.id.clone(),
                tx_hash,
                event_index,
                contract_id: event.contract_id.clone(),
                event_type: event.kind.clone(),
                payload,
            }
        })
        .collect()
}

/// A recorded backfill run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillRunRecord {
    /// The range that was replayed.
    pub range: LedgerRange,
    /// Whether the run finished.
    pub completed: bool,
    /// How many events were replayed.
    pub events_replayed: u64,
    /// The error, if the run failed.
    pub error: Option<String>,
}

/// Errors raised by the indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexerError {
    /// The node could not be reached or answered with an error.
    Rpc(RpcError),
    /// The database rejected a write.
    Database(String),
    /// The node does not serve events at all.
    NoEventsServed,
    /// A range was not usable.
    InvalidRange(String),
    /// The indexer is misconfigured.
    InvalidConfiguration(String),
}

impl std::fmt::Display for IndexerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(err) => write!(f, "indexer rpc error: {err}"),
            Self::Database(msg) => write!(f, "indexer database error: {msg}"),
            Self::NoEventsServed => write!(f, "the node does not serve any events"),
            Self::InvalidRange(msg) => write!(f, "invalid ledger range: {msg}"),
            Self::InvalidConfiguration(msg) => {
                write!(f, "invalid indexer configuration: {msg}")
            }
        }
    }
}

impl std::error::Error for IndexerError {}

impl From<RpcError> for IndexerError {
    fn from(error: RpcError) -> Self {
        Self::Rpc(error)
    }
}

/// The projection store the indexer writes to.
#[async_trait]
pub trait ProjectionStore: Send + Sync {
    /// Write a batch of decoded events. Returns how many rows were actually
    /// written; events that were already stored are skipped.
    async fn write_events(&self, events: &[DecodedChainEvent]) -> Result<usize, IndexerError>;

    /// Record that a range of ledgers has been processed.
    async fn record_range(
        &self,
        range: LedgerRange,
        source: RangeSource,
        event_count: u64,
    ) -> Result<(), IndexerError>;

    /// The ranges that have been processed.
    async fn ranges(&self) -> Result<Vec<LedgerRange>, IndexerError>;

    /// Record a detected gap.
    async fn record_gap(&self, gap: &LedgerGap) -> Result<(), IndexerError>;

    /// Record a backfill run.
    async fn record_backfill_run(&self, run: &BackfillRunRecord) -> Result<(), IndexerError>;

    /// How many events are stored.
    async fn event_count(&self) -> Result<u64, IndexerError>;
}

/// The SQLite-backed projection store.
#[derive(Clone)]
pub struct SqliteProjectionStore {
    pool: SqlitePool,
    stream: String,
}

impl SqliteProjectionStore {
    /// Create a store writing to the given database.
    pub fn new(pool: SqlitePool, stream: impl Into<String>) -> Self {
        Self {
            pool,
            stream: stream.into(),
        }
    }

    /// The stream this store writes to.
    pub fn stream(&self) -> &str {
        &self.stream
    }
}

#[async_trait]
impl ProjectionStore for SqliteProjectionStore {
    async fn write_events(&self, events: &[DecodedChainEvent]) -> Result<usize, IndexerError> {
        if events.is_empty() {
            return Ok(0);
        }

        let rows = dedupe_projections(project_events(events));
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| IndexerError::Database(e.to_string()))?;

        let mut written = 0usize;
        for event in events {
            let result = sqlx::query(
                "INSERT INTO indexed_events (
                     stream, paging_token, ledger, tx_hash, event_index,
                     contract_id, event_type, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (stream, tx_hash, event_index) DO NOTHING",
            )
            .bind(&self.stream)
            .bind(&event.paging_token)
            .bind(event.ledger as i64)
            .bind(&event.tx_hash)
            .bind(event.event_index as i64)
            .bind(&event.contract_id)
            .bind(&event.event_type)
            .bind(event.payload.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| IndexerError::Database(e.to_string()))?;
            written += result.rows_affected() as usize;
        }

        for row in rows {
            sqlx::query(
                "INSERT INTO event_projections (
                     stream, tx_hash, event_index, ledger, projection, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (stream, tx_hash, event_index) DO NOTHING",
            )
            .bind(&self.stream)
            .bind(&row.event.tx_hash)
            .bind(row.event.event_index as i64)
            .bind(row.ledger as i64)
            .bind(&row.projection)
            .bind(row.payload.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| IndexerError::Database(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| IndexerError::Database(e.to_string()))?;

        Ok(written)
    }

    async fn record_range(
        &self,
        range: LedgerRange,
        source: RangeSource,
        event_count: u64,
    ) -> Result<(), IndexerError> {
        sqlx::query(
            "INSERT INTO indexed_ledger_ranges (
                 stream, from_ledger, to_ledger, source, status, event_count, completed_at
             ) VALUES (?1, ?2, ?3, ?4, 'complete', ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT (stream, from_ledger, to_ledger) DO UPDATE SET
                 status = 'complete',
                 event_count = excluded.event_count,
                 source = excluded.source,
                 completed_at = excluded.completed_at",
        )
        .bind(&self.stream)
        .bind(range.from as i64)
        .bind(range.to as i64)
        .bind(source.as_str())
        .bind(event_count as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| IndexerError::Database(e.to_string()))?;
        Ok(())
    }

    async fn ranges(&self) -> Result<Vec<LedgerRange>, IndexerError> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT from_ledger, to_ledger FROM indexed_ledger_ranges
             WHERE stream = ?1 AND status = 'complete'",
        )
        .bind(&self.stream)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| IndexerError::Database(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(from, to)| LedgerRange::new(from as u32, to as u32))
            .collect())
    }

    async fn record_gap(&self, gap: &LedgerGap) -> Result<(), IndexerError> {
        sqlx::query(
            "INSERT INTO indexer_ledger_gaps (stream, from_ledger, to_ledger, details)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&self.stream)
        .bind(gap.from as i64)
        .bind(gap.to as i64)
        .bind(format!("{} ledgers missing", gap.len()))
        .execute(&self.pool)
        .await
        .map_err(|e| IndexerError::Database(e.to_string()))?;
        Ok(())
    }

    async fn record_backfill_run(&self, run: &BackfillRunRecord) -> Result<(), IndexerError> {
        sqlx::query(
            "INSERT INTO indexer_backfill_runs (
                 stream, from_ledger, to_ledger, status, events_replayed, error, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        )
        .bind(&self.stream)
        .bind(run.range.from as i64)
        .bind(run.range.to as i64)
        .bind(if run.completed { "complete" } else { "failed" })
        .bind(run.events_replayed as i64)
        .bind(run.error.clone())
        .execute(&self.pool)
        .await
        .map_err(|e| IndexerError::Database(e.to_string()))?;
        Ok(())
    }

    async fn event_count(&self) -> Result<u64, IndexerError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM indexed_events WHERE stream = ?1")
                .bind(&self.stream)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| IndexerError::Database(e.to_string()))?;
        Ok(count as u64)
    }
}

/// Counters describing what the indexer has done.
#[derive(Debug, Default)]
pub struct IndexerMetrics {
    events_indexed: AtomicU64,
    ranges_indexed: AtomicU64,
    gaps_detected: AtomicU64,
    gap_depth_ledgers: AtomicU64,
    backfilled_ranges: AtomicU64,
    backfilled_ledgers: AtomicU64,
    backfilled_events: AtomicU64,
    replay_failures: AtomicU64,
}

impl IndexerMetrics {
    /// A snapshot of the counters.
    pub fn snapshot(&self) -> IndexerMetricsSnapshot {
        IndexerMetricsSnapshot {
            events_indexed: self.events_indexed.load(Ordering::Relaxed),
            ranges_indexed: self.ranges_indexed.load(Ordering::Relaxed),
            gaps_detected: self.gaps_detected.load(Ordering::Relaxed),
            gap_depth_ledgers: self.gap_depth_ledgers.load(Ordering::Relaxed),
            backfilled_ranges: self.backfilled_ranges.load(Ordering::Relaxed),
            backfilled_ledgers: self.backfilled_ledgers.load(Ordering::Relaxed),
            backfilled_events: self.backfilled_events.load(Ordering::Relaxed),
            replay_failures: self.replay_failures.load(Ordering::Relaxed),
        }
    }

    fn record_gap_depth(&self, depth: u64) {
        self.gap_depth_ledgers.store(depth, Ordering::Relaxed);
    }

    /// Record a range that was replayed by a backfill.
    pub fn record_backfilled_range(&self, ledgers: u64, events: u64) {
        self.backfilled_ranges.fetch_add(1, Ordering::Relaxed);
        self.backfilled_ledgers
            .fetch_add(ledgers, Ordering::Relaxed);
        self.backfilled_events.fetch_add(events, Ordering::Relaxed);
    }

    /// Record a replay that failed.
    pub fn record_replay_failure(&self) {
        self.replay_failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// A snapshot of the indexer counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerMetricsSnapshot {
    /// Events written to the store.
    pub events_indexed: u64,
    /// Ledger ranges written to the store.
    pub ranges_indexed: u64,
    /// Number of open gaps.
    pub gaps_detected: u64,
    /// Total number of ledgers missing across all open gaps.
    pub gap_depth_ledgers: u64,
    /// Ranges replayed by a backfill.
    pub backfilled_ranges: u64,
    /// Ledgers replayed by a backfill.
    pub backfilled_ledgers: u64,
    /// Events replayed by a backfill.
    pub backfilled_events: u64,
    /// Number of failed replays.
    pub replay_failures: u64,
}

/// Whether the indexer is in a state where its data can be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerReadiness {
    /// Whether the indexer caught up and has no unresolved gap.
    pub ready: bool,
    /// Number of ledgers missing across all open gaps.
    pub gap_depth_ledgers: u64,
    /// Number of open gaps.
    pub open_gaps: usize,
    /// The newest ledger the node has closed.
    pub tip: Option<u32>,
    /// The newest ledger that has been indexed.
    pub last_indexed_ledger: Option<u32>,
    /// The oldest ledger that has been indexed.
    pub oldest_indexed_ledger: Option<u32>,
    /// Why the indexer is not ready, if it is not.
    pub reason: Option<String>,
}

/// Configuration of the live indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerConfig {
    /// The name of the stream.
    pub stream: String,
    /// How often the tip is polled.
    pub poll_interval: Duration,
    /// How many ledgers are fetched per event page.
    pub page_limit: u32,
    /// The contracts to index (empty means every contract).
    pub contract_ids: Vec<String>,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            stream: DEFAULT_STREAM.to_string(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            page_limit: DEFAULT_PAGE_LIMIT,
            contract_ids: Vec::new(),
        }
    }
}

impl IndexerConfig {
    /// Read the configuration from the environment.
    ///
    /// * `ZENITH_INDEXER_STREAM` — the name of the stream;
    /// * `ZENITH_INDEXER_POLL_INTERVAL_SECS` — how often the tip is polled;
    /// * `ZENITH_INDEXER_PAGE_LIMIT` — how many ledgers are fetched per page.
    pub fn from_env() -> Result<Self, IndexerError> {
        let mut config = Self::default();

        if let Some(stream) = read_env_optional("ZENITH_INDEXER_STREAM")? {
            if !stream.trim().is_empty() {
                config.stream = stream.trim().to_string();
            }
        }
        if let Some(secs) = read_env_optional::<u64>("ZENITH_INDEXER_POLL_INTERVAL_SECS")? {
            config.poll_interval = Duration::from_secs(secs.max(1));
        }
        if let Some(limit) = read_env_optional::<u32>("ZENITH_INDEXER_PAGE_LIMIT")? {
            config.page_limit = limit.max(1);
        }

        Ok(config)
    }
}

/// Read a parameter from the environment, or `None` when it is unset.
fn read_env_optional<T>(name: &str) -> Result<Option<T>, IndexerError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let value = trimmed.parse::<T>().map_err(|err| {
                IndexerError::InvalidConfiguration(format!("{name} is not a valid number: {err}"))
            })?;
            Ok(Some(value))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(IndexerError::InvalidConfiguration(format!(
            "{name} could not be read: {err}"
        ))),
    }
}

/// What one pass of the indexer did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngestReport {
    /// The ledger range that was covered.
    pub range: Option<LedgerRange>,
    /// How many events were written.
    pub events: usize,
    /// The gaps that are still open.
    pub gaps: Vec<LedgerGap>,
}

/// The live indexer: follows the tip and keeps the coverage up to date.
#[derive(Clone)]
pub struct ChainIndexer {
    rpc: Arc<dyn SorobanRpc>,
    store: Arc<dyn ProjectionStore>,
    config: IndexerConfig,
    metrics: Arc<IndexerMetrics>,
}

impl ChainIndexer {
    /// Create an indexer.
    pub fn new(
        rpc: Arc<dyn SorobanRpc>,
        store: Arc<dyn ProjectionStore>,
        config: IndexerConfig,
    ) -> Self {
        Self {
            rpc,
            store,
            config,
            metrics: Arc::new(IndexerMetrics::default()),
        }
    }

    /// The counters of this indexer.
    pub fn metrics(&self) -> &Arc<IndexerMetrics> {
        &self.metrics
    }

    /// The configuration of this indexer.
    pub fn config(&self) -> &IndexerConfig {
        &self.config
    }

    /// Index everything up to the current tip.
    pub async fn ingest_once(&self) -> Result<IngestReport, IndexerError> {
        let tip = self.rpc.latest_ledger().await?;
        let ranges = self.store.ranges().await?;
        let merged = merge_ranges(&ranges);

        let start = merged
            .iter()
            .rev()
            .find(|range| range.to <= tip.saturating_add(1))
            .map(|range| range.to)
            .unwrap_or(merged.first().map(|range| range.from).unwrap_or(0));

        if start > tip {
            let gaps = detect_gaps(&merged, tip);
            self.record_gaps(&gaps).await?;
            return Ok(IngestReport {
                range: None,
                events: 0,
                gaps,
            });
        }

        let params = GetEventsParams {
            start_ledger: start,
            end_ledger: Some(tip),
            cursor: None,
            limit: Some(self.config.page_limit),
            contract_ids: if self.config.contract_ids.is_empty() {
                None
            } else {
                Some(self.config.contract_ids.clone())
            },
        };

        let page = self.rpc.get_events(&params).await?;
        let events = decode_events(&page.events);
        let written = self.store.write_events(&events).await?;
        let range = LedgerRange::new(start, tip.saturating_add(1));
        self.store
            .record_range(range, RangeSource::Live, written as u64)
            .await?;

        self.metrics
            .events_indexed
            .fetch_add(written as u64, Ordering::Relaxed);
        self.metrics.ranges_indexed.fetch_add(1, Ordering::Relaxed);

        let merged = merge_ranges(&self.store.ranges().await?);
        let gaps = detect_gaps(&merged, tip);
        self.record_gaps(&gaps).await?;

        Ok(IngestReport {
            range: Some(range),
            events: written,
            gaps,
        })
    }

    /// Follow the tip until the task is cancelled.
    pub async fn follow_tip(&self) -> Result<(), IndexerError> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(err) = self.ingest_once().await {
                tracing::warn!(error = %err, "indexer pass failed");
            }
        }
    }

    /// The readiness of the indexer.
    pub async fn readiness(&self) -> IndexerReadiness {
        let tip = self.rpc.latest_ledger().await.ok();
        let ranges = self.store.ranges().await.unwrap_or_default();
        let merged = merge_ranges(&ranges);
        let gaps = match tip {
            Some(tip) => detect_gaps(&merged, tip),
            None => Vec::new(),
        };
        let depth: u64 = gaps.iter().map(|gap| gap.len() as u64).sum();
        let last_indexed_ledger = merged.iter().map(|range| range.to).max();
        let oldest_indexed_ledger = merged.iter().map(|range| range.from).min();

        let ready = gaps.is_empty();
        let reason = if ready {
            None
        } else {
            Some(format!(
                "{} ledgers missing across {} gaps",
                depth,
                gaps.len()
            ))
        };

        // The readiness check also refreshes the gap counters, so the metrics
        // and the readiness signal never disagree.
        self.update_gap_metrics(&gaps);

        IndexerReadiness {
            ready,
            gap_depth_ledgers: depth,
            open_gaps: gaps.len(),
            tip,
            last_indexed_ledger,
            oldest_indexed_ledger,
            reason,
        }
    }

    /// Refresh the gap counters without touching the database.
    fn update_gap_metrics(&self, gaps: &[LedgerGap]) {
        let depth: u64 = gaps.iter().map(|gap| gap.len() as u64).sum();
        self.metrics.record_gap_depth(depth);
        self.metrics
            .gaps_detected
            .store(gaps.len() as u64, Ordering::Relaxed);
    }

    async fn record_gaps(&self, gaps: &[LedgerGap]) -> Result<(), IndexerError> {
        self.update_gap_metrics(gaps);
        for gap in gaps {
            self.store.record_gap(gap).await?;
        }
        Ok(())
    }
}

/// Fetch one page of events and decode it.
pub async fn fetch_events(
    rpc: &dyn SorobanRpc,
    params: &GetEventsParams,
) -> Result<Vec<DecodedChainEvent>, IndexerError> {
    let page: GetEventsResponse = rpc.get_events(params).await?;
    Ok(decode_events(&page.events))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(from: u32, to: u32) -> LedgerRange {
        LedgerRange::new(from, to)
    }

    #[test]
    fn adjacent_ranges_are_merged() {
        let merged = merge_ranges(&[range(0, 10), range(10, 20), range(30, 40)]);
        assert_eq!(merged, vec![range(0, 20), range(30, 40)]);
    }

    #[test]
    fn overlapping_ranges_are_merged() {
        let merged = merge_ranges(&[range(0, 15), range(10, 20)]);
        assert_eq!(merged, vec![range(0, 20)]);
    }

    #[test]
    fn a_gap_between_ranges_is_detected() {
        let gaps = detect_gaps(&[range(0, 10), range(20, 30)], 29);
        assert_eq!(gaps, vec![LedgerGap { from: 10, to: 20 }]);
    }

    #[test]
    fn a_missing_tail_is_detected() {
        let gaps = detect_gaps(&[range(0, 10)], 20);
        assert_eq!(gaps, vec![LedgerGap { from: 10, to: 21 }]);
    }

    #[test]
    fn a_complete_coverage_has_no_gap() {
        assert!(detect_gaps(&[range(0, 21)], 20).is_empty());
    }

    #[test]
    fn gaps_are_reported_once_per_hole() {
        let gaps = detect_gaps(&[range(0, 5), range(10, 15)], 20);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].len(), 5);
        assert_eq!(gaps[1].len(), 6);
    }

    #[test]
    fn overlaps_are_reported() {
        let overlaps = detect_overlaps(&[range(0, 10), range(5, 20)]);
        assert_eq!(overlaps, vec![range(5, 10)]);
    }

    #[test]
    fn the_projection_is_a_pure_function() {
        let event = DecodedChainEvent {
            ledger: 7,
            paging_token: "000000007-1".into(),
            tx_hash: "hash".into(),
            event_index: 0,
            contract_id: "contract".into(),
            event_type: "OptionMinted".into(),
            payload: serde_json::json!({
                "series_id": "XLM-CALL",
                "strike": 0.12,
                "expiry": 1_700_000_000u64,
                "is_call": true,
                "contracts": 10.0,
                "writer": "GWRITER",
            }),
        };

        let rows = project_events(std::slice::from_ref(&event));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].projection, "option_minted");
        assert_eq!(rows[0].payload["series_id"], "XLM-CALL");
        // The same input always produces the same output.
        assert_eq!(rows, project_events(std::slice::from_ref(&event)));
    }

    #[test]
    fn unknown_events_are_not_projected() {
        let event = DecodedChainEvent {
            ledger: 7,
            paging_token: "000000007-1".into(),
            tx_hash: "hash".into(),
            event_index: 0,
            contract_id: "contract".into(),
            event_type: "SomethingElse".into(),
            payload: serde_json::json!({}),
        };
        assert!(project_events(&[event]).is_empty());
    }

    #[test]
    fn duplicated_projections_are_removed() {
        let event = DecodedChainEvent {
            ledger: 7,
            paging_token: "000000007-1".into(),
            tx_hash: "hash".into(),
            event_index: 0,
            contract_id: "contract".into(),
            event_type: "OptionMinted".into(),
            payload: serde_json::json!({
                "series_id": "XLM-CALL",
                "strike": 0.12,
                "expiry": 1_700_000_000u64,
                "is_call": true,
                "contracts": 10.0,
                "writer": "GWRITER",
            }),
        };
        let rows = project_events(&[event.clone(), event.clone()]);
        assert_eq!(rows.len(), 2);
        assert_eq!(dedupe_projections(rows).len(), 1);
    }
}
