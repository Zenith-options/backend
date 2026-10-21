//! Dynamic inclusion-fee strategy and the rolling daily fee budget.
//!
//! A Soroban transaction is only included if its inclusion fee is competitive
//! with the other transactions competing for the same ledger. A static fee is
//! wrong in both directions: too low and the transaction is dropped during
//! congestion (settlements land late), too high and the protocol systematically
//! overpays on every settlement.
//!
//! The strategy implemented here bids the *percentile* of the inclusion fees
//! recently observed on the network, clamped between a floor and a ceiling:
//!
//! * the **floor** keeps the bid above the point where the network would
//!   simply drop the transaction;
//! * the **ceiling** keeps a single transaction from paying an unbounded
//!   amount when the network is briefly pathological;
//! * the **per-transaction cap** bounds the *total* fee the protocol pays for
//!   one transaction (resource fee + inclusion fee);
//! * the **daily budget** bounds how much the protocol may pay over a rolling
//!   24 hour window. Once it is exhausted, submissions are blocked instead of
//!   silently overspending.
//!
//! When the node does not serve fee statistics (older testnet nodes), the
//! strategy falls back to the floor rather than failing the submission.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::rpc::{RpcError, SorobanRpc};
use super::types::GetFeeStatsResponse;

/// Percentile of the recent inclusion-fee distribution that is bid by default.
pub const DEFAULT_PERCENTILE: u8 = 90;

/// Default floor for the inclusion-fee bid, in stroops (1 XLM = 1_000_000).
///
/// Roughly the base fee of a transaction: enough to be included when the
/// network is calm, cheap enough to be irrelevant next to the resource fee.
pub const DEFAULT_FLOOR_STROOPS: i64 = 100;

/// Default ceiling for the inclusion-fee bid, in stroops (0.1 XLM).
///
/// A bid above this is never worth it for a settlement transaction.
pub const DEFAULT_CEILING_STROOPS: i64 = 100_000;

/// Default cap on the total fee (resource + inclusion) paid per transaction,
/// in stroops (0.5 XLM).
///
/// The cap covers the *total* fee, so it has to leave room for the resource
/// fee a real transaction declares: a transaction that reads and writes a
/// handful of ledger entries already costs a few hundred thousand stroops in
/// resources.
pub const DEFAULT_PER_TRANSACTION_CAP_STROOPS: i64 = 500_000;

/// Default rolling 24h budget for inclusion fees, in stroops (5 XLM).
pub const DEFAULT_DAILY_BUDGET_STROOPS: i64 = 5_000_000;

/// Length of the rolling budget window.
pub const DEFAULT_BUDGET_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a cached fee-statistics sample stays fresh.
pub const DEFAULT_FEE_CACHE_TTL: Duration = Duration::from_secs(30);

/// How often the background poller refreshes the cached sample.
pub const DEFAULT_FEE_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// A snapshot of the inclusion fees recently observed on the network.
///
/// All amounts are in stroops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeStats {
    /// Ledger the sample was taken at.
    pub latest_ledger: u32,
    /// Lowest inclusion fee observed in the window.
    pub min_fee: i64,
    /// Highest inclusion fee observed in the window.
    pub max_fee: i64,
    /// Median inclusion fee.
    pub p50: i64,
    /// 75th percentile inclusion fee.
    pub p75: i64,
    /// 90th percentile inclusion fee.
    pub p90: i64,
    /// 95th percentile inclusion fee.
    pub p95: i64,
    /// 99th percentile inclusion fee.
    pub p99: i64,
    /// Number of transactions in the sample.
    pub transaction_count: u64,
}

impl FeeStats {
    /// The inclusion fee at the requested percentile.
    ///
    /// Percentiles that the node does not report fall back to the closest
    /// reported one, so a node that only publishes `p50`/`p90` still works.
    pub fn percentile(&self, percentile: u8) -> i64 {
        match percentile {
            0..=50 => self.p50,
            51..=75 => self.p75,
            76..=90 => self.p90,
            91..=95 => self.p95,
            _ => self.p99,
        }
    }
}

impl From<GetFeeStatsResponse> for FeeStats {
    fn from(response: GetFeeStatsResponse) -> Self {
        let min_fee = response.min_fee.unwrap_or(0);
        let p50 = response.p50.unwrap_or(min_fee);
        let p75 = response.p75.unwrap_or(p50);
        let p90 = response.p90.unwrap_or(p75);
        let p95 = response.p95.unwrap_or(p90);
        let p99 = response.p99.unwrap_or(p95);
        Self {
            latest_ledger: response.latest_ledger.unwrap_or(0),
            min_fee,
            max_fee: response.max_fee.unwrap_or(p99),
            p50,
            p75,
            p90,
            p95,
            p99,
            transaction_count: response.transaction_count.unwrap_or(0),
        }
    }
}

/// Why fee statistics could not be obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeOracleError {
    /// The node could not be reached.
    Unavailable(String),
    /// The node does not serve fee statistics at all.
    NotSupported,
    /// The response could not be understood.
    Malformed(String),
}

impl std::fmt::Display for FeeOracleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(msg) => write!(f, "fee statistics unavailable: {msg}"),
            Self::NotSupported => write!(f, "node does not serve fee statistics"),
            Self::Malformed(msg) => write!(f, "malformed fee statistics: {msg}"),
        }
    }
}

impl std::error::Error for FeeOracleError {}

/// Source of the recent network fee statistics.
///
/// The production implementation polls the node; tests inject a deterministic
/// implementation so fee selection is reproducible.
#[async_trait]
pub trait FeeOracle: Send + Sync {
    /// The most recent fee statistics, or an error when they are unavailable.
    async fn fee_stats(&self) -> Result<FeeStats, FeeOracleError>;
}

/// A deterministic oracle backed by a fixed sample.
#[derive(Debug, Clone)]
pub struct StaticFeeOracle {
    stats: Option<FeeStats>,
}

impl StaticFeeOracle {
    /// An oracle that always returns the given sample.
    pub fn new(stats: FeeStats) -> Self {
        Self { stats: Some(stats) }
    }

    /// An oracle that never has statistics, like a node that does not serve
    /// `getFeeStats`. The strategy then falls back to its floor.
    pub fn unavailable() -> Self {
        Self { stats: None }
    }
}

#[async_trait]
impl FeeOracle for StaticFeeOracle {
    async fn fee_stats(&self) -> Result<FeeStats, FeeOracleError> {
        self.stats.ok_or(FeeOracleError::NotSupported)
    }
}

#[derive(Debug, Clone)]
struct CachedStats {
    stats: FeeStats,
    fetched_at: Instant,
}

/// The production oracle: polls `getFeeStats` on a short interval and serves
/// the cached sample in between.
pub struct RpcFeeOracle {
    rpc: Arc<dyn SorobanRpc>,
    cache: Mutex<Option<CachedStats>>,
    ttl: Duration,
    poll_interval: Duration,
}

impl std::fmt::Debug for RpcFeeOracle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcFeeOracle")
            .field("cached", &self.cached())
            .field("ttl", &self.ttl)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

impl RpcFeeOracle {
    /// Create an oracle backed by the given node.
    pub fn new(rpc: Arc<dyn SorobanRpc>) -> Self {
        Self {
            rpc,
            cache: Mutex::new(None),
            ttl: DEFAULT_FEE_CACHE_TTL,
            poll_interval: DEFAULT_FEE_POLL_INTERVAL,
        }
    }

    /// Create an oracle whose cache lifetime and poll interval come from the
    /// environment.
    ///
    /// * `ZENITH_FEE_POLL_INTERVAL_SECS` — how often the node is polled;
    /// * `ZENITH_FEE_CACHE_TTL_SECS` — how long a sample stays fresh.
    pub fn from_env(rpc: Arc<dyn SorobanRpc>) -> Result<Self, FeeStrategyError> {
        let mut oracle = Self::new(rpc);
        if let Some(secs) = read_env_optional::<u64>("ZENITH_FEE_POLL_INTERVAL_SECS")? {
            oracle.poll_interval = Duration::from_secs(secs.max(1));
        }
        if let Some(secs) = read_env_optional::<u64>("ZENITH_FEE_CACHE_TTL_SECS")? {
            oracle.ttl = Duration::from_secs(secs);
        }
        Ok(oracle)
    }

    /// Override how long a cached sample stays fresh.
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Override how often the background poller refreshes the sample.
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// The cached sample, if any, regardless of its age.
    pub fn cached(&self) -> Option<FeeStats> {
        self.lock().as_ref().map(|cached| cached.stats)
    }

    /// Force a refresh of the cached sample.
    pub async fn refresh(&self) -> Result<FeeStats, FeeOracleError> {
        let response = self.rpc.get_fee_stats().await.map_err(map_rpc_error)?;
        let stats = FeeStats::from(response);
        *self.lock() = Some(CachedStats {
            stats,
            fetched_at: Instant::now(),
        });
        Ok(stats)
    }

    /// Start a background task that keeps the cache warm.
    ///
    /// The task runs until the process exits; a failure to reach the node is
    /// logged and the previous sample is kept.
    pub fn spawn_poller(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let oracle = Arc::clone(self);
        let interval = self.poll_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(err) = oracle.refresh().await {
                    tracing::warn!(error = %err, "fee statistics refresh failed");
                }
            }
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<CachedStats>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh(&self) -> Option<FeeStats> {
        let guard = self.lock();
        guard
            .as_ref()
            .filter(|cached| cached.fetched_at.elapsed() < self.ttl)
            .map(|cached| cached.stats)
    }
}

#[async_trait]
impl FeeOracle for RpcFeeOracle {
    async fn fee_stats(&self) -> Result<FeeStats, FeeOracleError> {
        if let Some(stats) = self.fresh() {
            return Ok(stats);
        }
        self.refresh().await
    }
}

fn map_rpc_error(error: RpcError) -> FeeOracleError {
    match error {
        RpcError::MethodUnavailable(_) => FeeOracleError::NotSupported,
        RpcError::Decode(msg) => FeeOracleError::Malformed(msg),
        other => FeeOracleError::Unavailable(other.to_string()),
    }
}

/// Read a numeric parameter from the environment, falling back to a default.
fn read_env<T>(name: &str, default: T) -> Result<T, FeeStrategyError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(raw) => raw.trim().parse::<T>().map_err(|err| {
            FeeStrategyError::InvalidConfiguration(format!("{name} is not a valid number: {err}"))
        }),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(FeeStrategyError::InvalidConfiguration(format!(
            "{name} could not be read: {err}"
        ))),
    }
}

/// Read a numeric parameter from the environment, or `None` when it is unset.
fn read_env_optional<T>(name: &str) -> Result<Option<T>, FeeStrategyError>
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
                FeeStrategyError::InvalidConfiguration(format!(
                    "{name} is not a valid number: {err}"
                ))
            })?;
            Ok(Some(value))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(FeeStrategyError::InvalidConfiguration(format!(
            "{name} could not be read: {err}"
        ))),
    }
}

/// Where the chosen inclusion fee came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeSource {
    /// No statistics were available, the floor was used.
    Floor,
    /// The bid is the requested percentile of the recent fee distribution.
    Percentile,
    /// The bid was reduced to respect the per-transaction cap.
    Capped,
}

/// The inclusion fee the protocol is willing to pay for one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InclusionFee {
    /// The inclusion fee to bid, in stroops.
    pub amount_stroops: i64,
    /// The resource fee the transaction has to pay, in stroops.
    pub resource_fee_stroops: i64,
    /// Where the bid came from.
    pub source: FeeSource,
    /// The percentile the bid was taken from, when statistics were available.
    pub percentile: Option<u8>,
}

impl InclusionFee {
    /// Total fee the transaction pays (resource + inclusion), in stroops.
    pub fn total_stroops(&self) -> i64 {
        self.resource_fee_stroops + self.amount_stroops
    }
}

/// Errors raised by the fee strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeStrategyError {
    /// The strategy is misconfigured.
    InvalidConfiguration(String),
    /// The resource fee alone already exceeds the per-transaction cap.
    PerTransactionCapExceeded { resource_fee: i64, cap: i64 },
    /// The rolling daily budget is exhausted.
    BudgetExceeded {
        requested: i64,
        spent: i64,
        limit: i64,
    },
}

impl std::fmt::Display for FeeStrategyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(msg) => {
                write!(f, "invalid fee strategy configuration: {msg}")
            }
            Self::PerTransactionCapExceeded { resource_fee, cap } => write!(
                f,
                "resource fee {resource_fee} exceeds the per-transaction cap {cap}"
            ),
            Self::BudgetExceeded {
                requested,
                spent,
                limit,
            } => write!(
                f,
                "daily fee budget exhausted: {spent}/{limit} stroops spent, {requested} requested"
            ),
        }
    }
}

impl std::error::Error for FeeStrategyError {}

/// The configurable inclusion-fee strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InclusionFeeStrategy {
    /// Percentile of the recent inclusion-fee distribution to bid.
    pub percentile: u8,
    /// Lower bound of the inclusion-fee bid, in stroops.
    pub floor_stroops: i64,
    /// Upper bound of the inclusion-fee bid, in stroops.
    pub ceiling_stroops: i64,
    /// Upper bound of the *total* fee paid for one transaction, in stroops.
    pub per_transaction_cap_stroops: i64,
    /// Rolling 24h budget for inclusion fees, in stroops.
    pub daily_budget_stroops: i64,
}

impl Default for InclusionFeeStrategy {
    fn default() -> Self {
        Self {
            percentile: DEFAULT_PERCENTILE,
            floor_stroops: DEFAULT_FLOOR_STROOPS,
            ceiling_stroops: DEFAULT_CEILING_STROOPS,
            per_transaction_cap_stroops: DEFAULT_PER_TRANSACTION_CAP_STROOPS,
            daily_budget_stroops: DEFAULT_DAILY_BUDGET_STROOPS,
        }
    }
}

impl InclusionFeeStrategy {
    /// Read the strategy from the environment.
    ///
    /// Every parameter falls back to its default, so a deployment only has to
    /// set the ones it wants to change:
    ///
    /// * `ZENITH_FEE_PERCENTILE`
    /// * `ZENITH_FEE_FLOOR_STROOPS`
    /// * `ZENITH_FEE_CEILING_STROOPS`
    /// * `ZENITH_FEE_PER_TX_CAP_STROOPS`
    /// * `ZENITH_FEE_DAILY_BUDGET_STROOPS`
    pub fn from_env() -> Result<Self, FeeStrategyError> {
        let defaults = Self::default();
        let strategy = Self {
            percentile: read_env("ZENITH_FEE_PERCENTILE", defaults.percentile)?,
            floor_stroops: read_env("ZENITH_FEE_FLOOR_STROOPS", defaults.floor_stroops)?,
            ceiling_stroops: read_env("ZENITH_FEE_CEILING_STROOPS", defaults.ceiling_stroops)?,
            per_transaction_cap_stroops: read_env(
                "ZENITH_FEE_PER_TX_CAP_STROOPS",
                defaults.per_transaction_cap_stroops,
            )?,
            daily_budget_stroops: read_env(
                "ZENITH_FEE_DAILY_BUDGET_STROOPS",
                defaults.daily_budget_stroops,
            )?,
        };
        strategy.validate()?;
        Ok(strategy)
    }

    /// Check that the parameters can produce a sane fee.
    pub fn validate(&self) -> Result<(), FeeStrategyError> {
        if self.percentile == 0 || self.percentile > 100 {
            return Err(FeeStrategyError::InvalidConfiguration(format!(
                "percentile {} is out of range (1..=100)",
                self.percentile
            )));
        }
        if self.floor_stroops < 0 {
            return Err(FeeStrategyError::InvalidConfiguration(
                "floor must not be negative".to_string(),
            ));
        }
        if self.ceiling_stroops < self.floor_stroops {
            return Err(FeeStrategyError::InvalidConfiguration(format!(
                "ceiling {} is below the floor {}",
                self.ceiling_stroops, self.floor_stroops
            )));
        }
        if self.per_transaction_cap_stroops < 0 {
            return Err(FeeStrategyError::InvalidConfiguration(
                "per-transaction cap must not be negative".to_string(),
            ));
        }
        if self.daily_budget_stroops < 0 {
            return Err(FeeStrategyError::InvalidConfiguration(
                "daily budget must not be negative".to_string(),
            ));
        }
        Ok(())
    }

    /// Choose the inclusion fee for a transaction.
    ///
    /// * `stats` is the recent network fee sample. When it is `None` (the node
    ///   does not serve fee statistics) the floor is used.
    /// * `resource_fee` is the fee the transaction already has to pay for its
    ///   declared resources. The per-transaction cap applies to the *total*
    ///   fee, so the bid is reduced when the resource fee eats into it.
    pub fn select(
        &self,
        resource_fee: i64,
        stats: Option<&FeeStats>,
    ) -> Result<InclusionFee, FeeStrategyError> {
        self.validate()?;

        if resource_fee < 0 {
            return Err(FeeStrategyError::InvalidConfiguration(
                "resource fee must not be negative".to_string(),
            ));
        }
        if resource_fee > self.per_transaction_cap_stroops {
            return Err(FeeStrategyError::PerTransactionCapExceeded {
                resource_fee,
                cap: self.per_transaction_cap_stroops,
            });
        }

        let (mut amount, mut source, percentile) = match stats {
            Some(stats) => {
                let percentile = self.percentile;
                (
                    stats.percentile(percentile),
                    FeeSource::Percentile,
                    Some(percentile),
                )
            }
            // No statistics: bid the floor so the transaction is still
            // competitive without guessing.
            None => (self.floor_stroops, FeeSource::Floor, None),
        };

        // Clamp into [floor, ceiling].
        amount = amount.clamp(self.floor_stroops, self.ceiling_stroops);
        if amount == self.floor_stroops && source == FeeSource::Percentile {
            source = FeeSource::Floor;
        }

        // Respect the per-transaction cap on the total fee.
        let headroom = self.per_transaction_cap_stroops - resource_fee;
        if amount > headroom {
            amount = headroom;
            source = FeeSource::Capped;
        }

        Ok(InclusionFee {
            amount_stroops: amount,
            resource_fee_stroops: resource_fee,
            source,
            percentile,
        })
    }
}

/// A reservation against the rolling daily budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetReservation {
    /// The reserved amount, in stroops.
    pub amount_stroops: i64,
    /// Spend after the reservation was taken, in stroops.
    pub spent_after_stroops: i64,
    /// The budget limit, in stroops.
    pub limit_stroops: i64,
}

/// A rolling budget over a time window.
///
/// The budget is a moving window: spending that falls out of the window frees
/// the budget again, so a quiet day does not permanently exhaust it.
#[derive(Debug)]
pub struct DailyFeeBudget {
    limit_stroops: i64,
    window: Duration,
    entries: Mutex<Vec<(Instant, i64)>>,
}

impl DailyFeeBudget {
    /// A budget of `limit_stroops` over a rolling 24 hour window.
    pub fn new(limit_stroops: i64) -> Self {
        Self {
            limit_stroops,
            window: DEFAULT_BUDGET_WINDOW,
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Override the window length (used by tests to shrink it).
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// The budget limit, in stroops.
    pub fn limit_stroops(&self) -> i64 {
        self.limit_stroops
    }

    /// The length of the rolling window.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// How much has been spent inside the current window, in stroops.
    pub fn spent(&self) -> i64 {
        let now = Instant::now();
        self.prune(now)
    }

    /// How much of the budget is still available, in stroops.
    pub fn remaining(&self) -> i64 {
        (self.limit_stroops - self.spent()).max(0)
    }

    /// Reserve `amount` from the budget.
    ///
    /// Fails with [`FeeStrategyError::BudgetExceeded`] when the reservation
    /// would push the window over the limit; nothing is reserved in that case.
    pub fn try_reserve(&self, amount: i64) -> Result<BudgetReservation, FeeStrategyError> {
        if amount < 0 {
            return Err(FeeStrategyError::InvalidConfiguration(
                "reservation must not be negative".to_string(),
            ));
        }
        let now = Instant::now();
        let mut entries = self.lock();
        entries.retain(|(at, _)| now.duration_since(*at) < self.window);
        let spent: i64 = entries.iter().map(|(_, amount)| amount).sum();
        if spent + amount > self.limit_stroops {
            return Err(FeeStrategyError::BudgetExceeded {
                requested: amount,
                spent,
                limit: self.limit_stroops,
            });
        }
        entries.push((now, amount));
        Ok(BudgetReservation {
            amount_stroops: amount,
            spent_after_stroops: spent + amount,
            limit_stroops: self.limit_stroops,
        })
    }

    /// Give a reservation back, e.g. when the submission was rejected.
    pub fn release(&self, amount: i64) {
        let mut entries = self.lock();
        let position = entries.iter().rposition(|(_, booked)| *booked == amount);
        if let Some(position) = position {
            entries.remove(position);
        }
    }

    /// Forget every reservation (used by tests).
    pub fn reset(&self) {
        self.lock().clear();
    }

    fn prune(&self, now: Instant) -> i64 {
        let mut entries = self.lock();
        entries.retain(|(at, _)| now.duration_since(*at) < self.window);
        entries.iter().map(|(_, amount)| amount).sum()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(Instant, i64)>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FeeStats {
        FeeStats {
            latest_ledger: 1_000,
            min_fee: 100,
            max_fee: 900_000,
            p50: 200,
            p75: 400,
            p90: 800,
            p95: 1_200,
            p99: 5_000,
            transaction_count: 120,
        }
    }

    #[test]
    fn percentile_falls_back_to_the_closest_reported_bucket() {
        let stats = sample();
        assert_eq!(stats.percentile(50), 200);
        assert_eq!(stats.percentile(75), 400);
        assert_eq!(stats.percentile(90), 800);
        assert_eq!(stats.percentile(95), 1_200);
        assert_eq!(stats.percentile(99), 5_000);
    }

    #[test]
    fn selects_the_configured_percentile() {
        let strategy = InclusionFeeStrategy::default();
        let fee = strategy.select(1_000, Some(&sample())).unwrap();
        assert_eq!(fee.amount_stroops, 800);
        assert_eq!(fee.source, FeeSource::Percentile);
        assert_eq!(fee.percentile, Some(90));
        assert_eq!(fee.total_stroops(), 1_800);
    }

    #[test]
    fn falls_back_to_the_floor_without_statistics() {
        let strategy = InclusionFeeStrategy::default();
        let fee = strategy.select(1_000, None).unwrap();
        assert_eq!(fee.amount_stroops, DEFAULT_FLOOR_STROOPS);
        assert_eq!(fee.source, FeeSource::Floor);
        assert_eq!(fee.percentile, None);
    }

    #[test]
    fn clamps_the_bid_to_the_ceiling() {
        let strategy = InclusionFeeStrategy {
            ceiling_stroops: 600,
            ..Default::default()
        };
        let fee = strategy.select(0, Some(&sample())).unwrap();
        assert_eq!(fee.amount_stroops, 600);
    }

    #[test]
    fn clamps_the_bid_to_the_floor() {
        let strategy = InclusionFeeStrategy {
            floor_stroops: 1_000,
            ..Default::default()
        };
        let mut stats = sample();
        stats.p90 = 10;
        let fee = strategy.select(0, Some(&stats)).unwrap();
        assert_eq!(fee.amount_stroops, 1_000);
        assert_eq!(fee.source, FeeSource::Floor);
    }

    #[test]
    fn per_transaction_cap_reduces_the_bid() {
        let strategy = InclusionFeeStrategy {
            per_transaction_cap_stroops: 1_000,
            ..Default::default()
        };
        let fee = strategy.select(900, Some(&sample())).unwrap();
        assert_eq!(fee.amount_stroops, 100);
        assert_eq!(fee.source, FeeSource::Capped);
    }

    #[test]
    fn resource_fee_above_the_cap_is_rejected() {
        let strategy = InclusionFeeStrategy {
            per_transaction_cap_stroops: 1_000,
            ..Default::default()
        };
        let err = strategy.select(1_001, Some(&sample())).unwrap_err();
        assert_eq!(
            err,
            FeeStrategyError::PerTransactionCapExceeded {
                resource_fee: 1_001,
                cap: 1_000
            }
        );
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let strategy = InclusionFeeStrategy {
            percentile: 0,
            ..Default::default()
        };
        assert!(matches!(
            strategy.select(0, None),
            Err(FeeStrategyError::InvalidConfiguration(_))
        ));

        let strategy = InclusionFeeStrategy {
            floor_stroops: 500,
            ceiling_stroops: 100,
            ..Default::default()
        };
        assert!(matches!(
            strategy.select(0, None),
            Err(FeeStrategyError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn budget_blocks_once_it_is_exhausted() {
        let budget = DailyFeeBudget::new(1_000);
        let first = budget.try_reserve(600).unwrap();
        assert_eq!(first.spent_after_stroops, 600);
        assert_eq!(budget.remaining(), 400);

        let err = budget.try_reserve(500).unwrap_err();
        assert_eq!(
            err,
            FeeStrategyError::BudgetExceeded {
                requested: 500,
                spent: 600,
                limit: 1_000
            }
        );
        // The failed reservation was not booked.
        assert_eq!(budget.spent(), 600);
    }

    #[test]
    fn budget_window_rolls() {
        let budget = DailyFeeBudget::new(1_000).with_window(Duration::from_millis(20));
        budget.try_reserve(900).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(budget.spent(), 0);
        assert_eq!(budget.remaining(), 1_000);
    }

    #[test]
    fn released_reservations_free_the_budget() {
        let budget = DailyFeeBudget::new(1_000);
        budget.try_reserve(900).unwrap();
        budget.release(900);
        assert_eq!(budget.spent(), 0);
        assert!(budget.try_reserve(1_000).is_ok());
    }

    #[tokio::test]
    async fn static_oracle_reports_missing_statistics() {
        let oracle = StaticFeeOracle::unavailable();
        assert_eq!(oracle.fee_stats().await, Err(FeeOracleError::NotSupported));

        let oracle = StaticFeeOracle::new(sample());
        assert_eq!(oracle.fee_stats().await.unwrap().p90, 800);
    }
}
