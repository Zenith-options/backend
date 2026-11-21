//! Historical realized-volatility estimators.
//!
//! Three estimators, all pure functions over `&[Candle]`:
//!
//! * [`close_to_close`] — classic log-return standard deviation.
//! * [`parkinson`] — high/low range estimator.
//! * [`yang_zhang`] — drift-independent, combines overnight, open/close and
//!   Rogers–Satchell range terms.
//!
//! All results are annualised with a **365-day calendar** convention, since
//! crypto trades 24/7. Buckets are 1h candles, so there are 24 buckets per
//! day and `ANNUAL_BUCKETS = 365 * 24`.
//!
//! [`compute_cached`] memoises [`compute`] results in a bounded cache keyed by
//! `(series, window)` so the analytics endpoint does not recompute the full
//! series on every request. Call [`invalidate`] from the indexer write path
//! whenever new trades land for a series so a backfill never serves a stale
//! value.

use crate::candles::Candle;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Number of 1h buckets in a 365-day calendar year.
pub const ANNUAL_BUCKETS: f64 = 365.0 * 24.0;

/// Minimum fraction of a window's buckets that must contain data.
pub const MIN_COVERAGE: f64 = 0.80;

/// Supported rolling windows, expressed in 1h buckets.
pub const WINDOWS: [(&str, usize); 3] = [("7d", 7 * 24), ("30d", 30 * 24), ("90d", 90 * 24)];

/// Upper bound on cached `(series, window)` entries. When exceeded the cache is
/// cleared wholesale, which keeps memory bounded without an LRU dependency.
const CACHE_CAPACITY: usize = 1024;

/// A single realized-volatility result for one (underlying, window) pair.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RvResult {
    pub window: String,
    pub close_to_close: f64,
    pub parkinson: f64,
    pub yang_zhang: f64,
    pub buckets: usize,
    pub coverage: f64,
}

use serde::Serialize;

/// Bounded memo cache keyed by `(series, window)`.
fn cache() -> &'static Mutex<HashMap<(String, usize), Option<RvResult>>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, usize), Option<RvResult>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop every cached value for `series` (all windows). Call from the indexer
/// write path when new trades are indexed for that series.
pub fn invalidate(series: &str) {
    if let Ok(mut map) = cache().lock() {
        map.retain(|(s, _), _| s != series);
    }
}

/// Drop the entire cache. Useful after a bulk backfill across many series.
pub fn invalidate_all() {
    if let Ok(mut map) = cache().lock() {
        map.clear();
    }
}

/// Reject candles with non-positive or non-finite prices.
fn valid(c: &Candle) -> bool {
    c.open > 0.0
        && c.high > 0.0
        && c.low > 0.0
        && c.close > 0.0
        && c.open.is_finite()
        && c.high.is_finite()
        && c.low.is_finite()
        && c.close.is_finite()
}

/// Close-to-close estimator: sample std-dev of log returns, annualised.
/// Flat prices yield 0.0 (not NaN).
pub fn close_to_close(candles: &[Candle]) -> f64 {
    let closes: Vec<f64> = candles.iter().filter(|c| valid(c)).map(|c| c.close).collect();
    if closes.len() < 2 {
        return 0.0;
    }
    let rets: Vec<f64> = closes.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let n = rets.len() as f64;
    let mean = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (var.max(0.0)).sqrt() * ANNUAL_BUCKETS.sqrt()
}

/// Parkinson estimator using the high/low range, annualised.
pub fn parkinson(candles: &[Candle]) -> f64 {
    let cs: Vec<&Candle> = candles.iter().filter(|c| valid(c)).collect();
    if cs.is_empty() {
        return 0.0;
    }
    let n = cs.len() as f64;
    let sum: f64 = cs
        .iter()
        .map(|c| (c.high / c.low).ln().powi(2))
        .sum();
    let var = sum / (4.0 * n * (2.0_f64).ln());
    (var.max(0.0)).sqrt() * ANNUAL_BUCKETS.sqrt()
}

/// Yang–Zhang estimator: drift-independent, combines overnight, open/close
/// and Rogers–Satchell range terms, annualised.
pub fn yang_zhang(candles: &[Candle]) -> f64 {
    let cs: Vec<&Candle> = candles.iter().filter(|c| valid(c)).collect();
    if cs.len() < 2 {
        return 0.0;
    }
    let n = cs.len() as f64;

    // Overnight (close-to-open) log returns.
    let overnight: Vec<f64> = cs
        .windows(2)
        .map(|w| (w[1].open / w[0].close).ln())
        .collect();
    // Open-to-close log returns.
    let open_close: Vec<f64> = cs.iter().map(|c| (c.close / c.open).ln()).collect();

    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let var = |v: &[f64]| {
        let m = mean(v);
        v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() as f64 - 1.0)
    };

    let var_o = var(&overnight);
    let var_c = var(&open_close);

    // Rogers–Satchell range term.
    let rs: f64 = cs
        .iter()
        .map(|c| {
            let hl = (c.high / c.low).ln();
            let co = (c.close / c.open).ln();
            hl * co
        })
        .sum::<f64>()
        / n;

    let var_rs = rs.max(0.0);
    let var_yz = var_o + 0.5 * var_rs + (1.0 - 0.5) * var_c;
    (var_yz.max(0.0)).sqrt() * ANNUAL_BUCKETS.sqrt()
}

/// Compute all three estimators over the trailing `window` buckets.
/// Returns `None` when fewer than 80% of the window's buckets have data.
pub fn compute(candles: &[Candle], window: usize, label: &str) -> Option<RvResult> {
    let usable: Vec<Candle> = candles
        .iter()
        .filter(|c| valid(c))
        .cloned()
        .collect();
    let coverage = usable.len() as f64 / window as f64;
    if coverage < MIN_COVERAGE {
        return None;
    }
    Some(RvResult {
        window: label.to_string(),
        close_to_close: close_to_close(&usable),
        parkinson: parkinson(&usable),
        yang_zhang: yang_zhang(&usable),
        buckets: usable.len(),
        coverage,
    })
}

/// Cached variant of [`compute`], keyed by `(series, window)`.
///
/// The numeric result is identical to [`compute`]; only the amount of work per
/// request drops. The cache is invalidated via [`invalidate`] when new trades
/// are indexed for `series`.
pub fn compute_cached(
    series: &str,
    candles: &[Candle],
    window: usize,
    label: &str,
) -> Option<RvResult> {
    let key = (series.to_string(), window);
    if let Ok(map) = cache().lock() {
        if let Some(hit) = map.get(&key) {
            return hit.clone();
        }
    }
    let result = compute(candles, window, label);
    if let Ok(mut map) = cache().lock() {
        if map.len() >= CACHE_CAPACITY {
            map.clear();
        }
        map.insert(key, result.clone());
    }
    result
}
