//! Historical backfill: replaying the ledger ranges the indexer missed.
//!
//! The live indexer follows the tip, but a restart, an RPC outage or a
//! cursor regression leaves holes in the ledgers it has processed. This module
//! plans and executes the repair:
//!
//! * the gaps are turned into a bounded plan of ranges to replay,
//! * ranges the node no longer serves (its event retention window has passed)
//!   are reported as skipped instead of being silently dropped,
//! * the replay writes through the same projection step as the live indexer,
//!   so a replayed range produces exactly the same rows as the original pass.
//!
//! Because every event is keyed by transaction hash plus event index, a
//! replay that overlaps with the live indexer writes the same rows again and
//! changes nothing.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::chain::indexer::{
    decode_events, BackfillRunRecord, IndexerError, IndexerMetrics, LedgerGap, LedgerRange,
    ProjectionStore, RangeSource,
};
use crate::chain::rpc::SorobanRpc;
use crate::chain::types::GetEventsParams;

/// Default number of ledgers replayed per backfill range.
pub const DEFAULT_MAX_RANGE: u32 = 1_000;

/// Default number of ledgers fetched per event page.
pub const DEFAULT_PAGE_LIMIT: u32 = 200;

/// Default pause between two event pages, to stay inside the RPC rate limit.
pub const DEFAULT_RATE_LIMIT: Duration = Duration::from_millis(100);

/// Default pause before the first retry of a rate limited request.
pub const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(250);

/// How often a rate limited request is retried before the run gives up.
pub const DEFAULT_MAX_RETRIES: u32 = 5;

/// Why a range was not replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The node no longer serves events for that range: its retention window
    /// has moved past it.
    OutsideRetention {
        /// The oldest ledger the node still serves.
        oldest_available: u32,
    },
    /// The range is empty.
    Empty,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRetention { oldest_available } => write!(
                f,
                "the node only serves events from ledger {oldest_available} onwards"
            ),
            Self::Empty => write!(f, "the range is empty"),
        }
    }
}

/// A range that will not be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedRange {
    /// The range that was skipped.
    pub range: LedgerRange,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// The ranges to replay.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BackfillPlan {
    /// The ranges to replay, in ascending order.
    pub ranges: Vec<LedgerRange>,
    /// The ranges that will not be replayed.
    pub skipped: Vec<SkippedRange>,
}

impl BackfillPlan {
    /// Total number of ledgers the plan replays.
    pub fn total_ledgers(&self) -> u64 {
        self.ranges.iter().map(|range| range.len() as u64).sum()
    }
}

/// Turn the detected gaps into a plan.
///
/// * `oldest_available` is the oldest ledger the node still serves. Gaps that
///   start before it cannot be replayed and are reported as skipped.
/// * `max_range` bounds how many ledgers a single replay covers, so a huge
///   gap is replayed in slices instead of one enormous request.
pub fn plan_backfill(gaps: &[LedgerGap], oldest_available: u32, max_range: u32) -> BackfillPlan {
    let mut plan = BackfillPlan::default();
    let max_range = max_range.max(1);

    for gap in gaps {
        if gap.is_empty() {
            plan.skipped.push(SkippedRange {
                range: gap.range(),
                reason: SkipReason::Empty,
            });
            continue;
        }

        // The node only serves events from `oldest_available` onwards, so the
        // part of the gap before it can never be replayed.
        let start = gap.from.max(oldest_available);
        if start >= gap.to {
            plan.skipped.push(SkippedRange {
                range: gap.range(),
                reason: SkipReason::OutsideRetention { oldest_available },
            });
            continue;
        }

        // The part of the gap the node already forgot is reported, not
        // silently dropped.
        if gap.from < oldest_available {
            plan.skipped.push(SkippedRange {
                range: LedgerRange::new(gap.from, start),
                reason: SkipReason::OutsideRetention { oldest_available },
            });
        }

        let mut cursor = start;
        while cursor < gap.to {
            let end = cursor.saturating_add(max_range).min(gap.to);
            plan.ranges.push(LedgerRange::new(cursor, end));
            cursor = end;
        }
    }

    plan
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

/// What a backfill run did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BackfillReport {
    /// How many ranges were replayed.
    pub ranges_replayed: usize,
    /// How many ledgers were replayed.
    pub ledgers_replayed: u64,
    /// How many events were written.
    pub events_replayed: u64,
    /// The ranges that were not replayed.
    pub skipped: Vec<SkippedRange>,
    /// The ranges that failed, with the reason.
    pub failures: Vec<(LedgerRange, String)>,
}

/// Replays missed ledger ranges.
#[derive(Clone)]
pub struct BackfillRunner {
    rpc: Arc<dyn SorobanRpc>,
    store: Arc<dyn ProjectionStore>,
    metrics: Arc<IndexerMetrics>,
    max_range: u32,
    page_limit: u32,
    rate_limit: Duration,
    retry_delay: Duration,
    max_retries: u32,
}

impl BackfillRunner {
    /// Create a runner.
    pub fn new(rpc: Arc<dyn SorobanRpc>, store: Arc<dyn ProjectionStore>) -> Self {
        Self {
            rpc,
            store,
            metrics: Arc::new(IndexerMetrics::default()),
            max_range: DEFAULT_MAX_RANGE,
            page_limit: DEFAULT_PAGE_LIMIT,
            rate_limit: DEFAULT_RATE_LIMIT,
            retry_delay: DEFAULT_RETRY_DELAY,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// Share the counters with the live indexer.
    pub fn with_metrics(mut self, metrics: Arc<IndexerMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Create a runner whose parameters come from the environment.
    ///
    /// * `ZENITH_INDEXER_MAX_RANGE` — how many ledgers a single replay covers;
    /// * `ZENITH_INDEXER_PAGE_LIMIT` — how many ledgers are fetched per page;
    /// * `ZENITH_INDEXER_RATE_LIMIT_MS` — the pause between two pages.
    pub fn from_env(
        rpc: Arc<dyn SorobanRpc>,
        store: Arc<dyn ProjectionStore>,
    ) -> Result<Self, IndexerError> {
        let mut runner = Self::new(rpc, store);

        if let Some(max_range) = read_env_optional::<u32>("ZENITH_INDEXER_MAX_RANGE")? {
            runner.max_range = max_range.max(1);
        }
        if let Some(limit) = read_env_optional::<u32>("ZENITH_INDEXER_PAGE_LIMIT")? {
            runner.page_limit = limit.max(1);
        }
        if let Some(millis) = read_env_optional::<u64>("ZENITH_INDEXER_RATE_LIMIT_MS")? {
            runner.rate_limit = Duration::from_millis(millis);
        }

        Ok(runner)
    }

    /// Override how many ledgers a single replay covers.
    pub fn with_max_range(mut self, max_range: u32) -> Self {
        self.max_range = max_range.max(1);
        self
    }

    /// Override how many ledgers are fetched per page.
    pub fn with_page_limit(mut self, page_limit: u32) -> Self {
        self.page_limit = page_limit.max(1);
        self
    }

    /// Override the pause between two event pages.
    pub fn with_rate_limit(mut self, rate_limit: Duration) -> Self {
        self.rate_limit = rate_limit;
        self
    }

    /// Override the pause before the first retry of a rate limited request.
    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    /// Override how often a rate limited request is retried.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// The counters of this runner.
    pub fn metrics(&self) -> &Arc<IndexerMetrics> {
        &self.metrics
    }

    /// The oldest ledger the node still serves events for.
    ///
    /// The node's event retention is bounded, so a range that is too old can
    /// never be replayed. The floor is found with a binary search, which keeps
    /// the number of requests logarithmic.
    pub async fn detect_retention_floor(&self, tip: u32) -> Result<u32, IndexerError> {
        if !self.serves(tip).await {
            return Err(IndexerError::NoEventsServed);
        }
        let mut low = 1u32;
        let mut high = tip;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.serves(mid).await {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        Ok(low)
    }

    /// Whether the node still serves events for a ledger.
    pub async fn serves(&self, ledger: u32) -> bool {
        let params = GetEventsParams {
            start_ledger: ledger,
            end_ledger: Some(ledger),
            cursor: None,
            limit: Some(1),
            contract_ids: None,
        };
        self.rpc.get_events(&params).await.is_ok()
    }

    /// Replay the ranges of a plan.
    ///
    /// The ranges are replayed in ascending order, so the projection always
    /// moves forward. A range that fails is reported and the run continues
    /// with the next one: one bad range must not stop the repair.
    pub async fn run(&self, plan: &BackfillPlan) -> Result<BackfillReport, IndexerError> {
        let mut report = BackfillReport {
            skipped: plan.skipped.clone(),
            ..BackfillReport::default()
        };

        let mut ranges = plan.ranges.clone();
        ranges.sort_by_key(|range| (range.from, range.to));

        for range in ranges {
            match self.replay_range(&range).await {
                Ok(events) => {
                    report.ranges_replayed += 1;
                    report.ledgers_replayed += range.len() as u64;
                    report.events_replayed += events as u64;
                    self.metrics
                        .record_backfilled_range(range.len() as u64, events as u64);
                }
                Err(err) => {
                    self.metrics.record_replay_failure();
                    let message = err.to_string();
                    self.store
                        .record_backfill_run(&BackfillRunRecord {
                            range,
                            completed: false,
                            events_replayed: 0,
                            error: Some(message.clone()),
                        })
                        .await?;
                    report.failures.push((range, message));
                }
            }
        }

        Ok(report)
    }

    /// Fetch a page of events, backing off when the node rate limits us.
    ///
    /// Rate limiting is expected during a replay: the runner pauses and
    /// doubles the pause after every refused request, so a busy node slows
    /// the replay down instead of failing it.
    async fn fetch_page(
        &self,
        params: &GetEventsParams,
    ) -> Result<crate::chain::types::GetEventsResponse, IndexerError> {
        let mut delay = self.retry_delay;
        let mut attempt = 0u32;
        loop {
            match self.rpc.get_events(params).await {
                Ok(page) => return Ok(page),
                Err(err) if err.is_transient() && attempt < self.max_retries => {
                    attempt += 1;
                    tracing::debug!(
                        attempt,
                        error = %err,
                        "the node rate limited the replay, backing off"
                    );
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    delay *= 2;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Replay a single range and record it.
    async fn replay_range(&self, range: &LedgerRange) -> Result<u64, IndexerError> {
        let mut cursor: Option<String> = None;
        let mut previous_cursor: Option<String> = None;
        let mut written = 0u64;

        loop {
            let params = GetEventsParams {
                start_ledger: range.from,
                end_ledger: Some(range.to.saturating_sub(1)),
                cursor: cursor.clone(),
                limit: Some(self.page_limit),
                contract_ids: None,
            };

            let page = self.fetch_page(&params).await?;
            let events = decode_events(&page.events);
            written += self.store.write_events(&events).await? as u64;

            let next_cursor = page.cursor.clone();
            let exhausted = next_cursor.is_none() || page.events.is_empty();
            // Guard against a node that keeps handing back the same cursor.
            let repeated = next_cursor.is_some() && next_cursor == previous_cursor;
            cursor = next_cursor;
            if exhausted || repeated {
                break;
            }
            previous_cursor = cursor.clone();

            if !self.rate_limit.is_zero() {
                tokio::time::sleep(self.rate_limit).await;
            }
        }

        self.store
            .record_range(*range, RangeSource::Backfill, written)
            .await?;
        self.store
            .record_backfill_run(&BackfillRunRecord {
                range: *range,
                completed: true,
                events_replayed: written,
                error: None,
            })
            .await?;

        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::indexer::detect_gaps;

    fn gap(from: u32, to: u32) -> LedgerGap {
        LedgerGap { from, to }
    }

    #[test]
    fn a_gap_is_split_into_bounded_ranges() {
        let plan = plan_backfill(&[gap(100, 250)], 0, 100);
        assert_eq!(plan.ranges.len(), 2);
        assert_eq!(plan.ranges[0], LedgerRange::new(100, 200));
        assert_eq!(plan.ranges[1], LedgerRange::new(200, 250));
        assert_eq!(plan.total_ledgers(), 150);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn ranges_outside_the_retention_window_are_skipped() {
        let plan = plan_backfill(&[gap(10, 20)], 100, 100);
        assert!(plan.ranges.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(
            plan.skipped[0].reason,
            SkipReason::OutsideRetention {
                oldest_available: 100
            }
        );
    }

    #[test]
    fn a_gap_that_starts_inside_the_retention_window_is_replayed() {
        let plan = plan_backfill(&[gap(90, 120)], 100, 100);
        assert_eq!(plan.ranges, vec![LedgerRange::new(100, 120)]);
    }

    #[test]
    fn gaps_are_reported_in_ascending_order() {
        let gaps = detect_gaps(&[LedgerRange::new(0, 10)], 100);
        let plan = plan_backfill(&gaps, 0, 1_000);
        assert_eq!(plan.ranges, vec![LedgerRange::new(10, 101)]);
    }

    #[test]
    fn empty_gaps_are_skipped() {
        let plan = plan_backfill(&[gap(10, 10)], 0, 100);
        assert!(plan.ranges.is_empty());
        assert_eq!(plan.skipped[0].reason, SkipReason::Empty);
    }
}
