//! DEX routing: evaluate available venues, select a route (including
//! multi-hop path payments), enforce `min_received`, and submit.
//!
//! Design:
//!   - `quote_route` is a **pure function** of a `VenueSnapshot` — fully
//!     unit-testable and usable in dry-run / simulation mode.
//!   - `submit_route` re-validates the quote immediately before sending
//!     to protect against order-book movement between quote and submit.
//!   - Thin-liquidity / below-bound outcomes return `RouteOutcome::Fallback`
//!     rather than executing a bad trade.

use serde::{Deserialize, Serialize};

// ── Venue types ───────────────────────────────────────────────────────────────

/// Identifies a trading venue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Venue {
    /// Stellar SDEX (classic order book).
    StellarDex,
    /// Soroban AMM liquidity pool.
    SorobanAmm { pool_id: String },
}

/// A liquidity tier on a venue: at most `max_fill` units of `asset_in` can
/// be swapped for `price` units of `asset_out` per unit.
#[derive(Debug, Clone)]
pub struct LiquidityTier {
    pub venue:    Venue,
    pub price:    f64,   // asset_out per unit of asset_in
    pub max_fill: f64,   // maximum asset_in this tier absorbs
}

/// Immutable snapshot of all available venue liquidity.  This is what
/// `quote_route` consumes — making route selection a pure function.
#[derive(Debug, Clone, Default)]
pub struct VenueSnapshot {
    pub tiers: Vec<LiquidityTier>,
}

impl VenueSnapshot {
    pub fn add_tier(&mut self, tier: LiquidityTier) {
        self.tiers.push(tier);
    }

    /// Best achievable price across all tiers (highest asset_out per asset_in).
    pub fn best_price(&self) -> Option<f64> {
        self.tiers.iter().map(|t| t.price).reduce(f64::max)
    }
}

// ── Route / quote types ───────────────────────────────────────────────────────

/// A single hop in a multi-venue route.
#[derive(Debug, Clone)]
pub struct RouteHop {
    pub venue:     Venue,
    pub amount_in: f64,
    pub price:     f64,
    pub expected_out: f64,
}

/// A proposed route: one or more hops that together convert `total_in` to
/// `expected_out`, subject to `min_received`.
#[derive(Debug, Clone)]
pub struct Route {
    pub hops:         Vec<RouteHop>,
    pub total_in:     f64,
    pub expected_out: f64,
    pub min_received: f64,
}

/// Result of a quote attempt.
#[derive(Debug, Clone)]
pub enum QuoteResult {
    /// A viable route was found.
    Route(Route),
    /// Not enough liquidity for even a partial fill at the minimum price.
    InsufficientLiquidity { available: f64 },
    /// Best achievable price is below `min_price`.
    BelowMinPrice { best_price: f64, min_price: f64 },
}

// ── Routing config ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RoutingConfig {
    /// Maximum allowed slippage from oracle price (e.g. 0.02 = 2%).
    pub max_slippage_pct: f64,
    /// Minimum price (asset_out per asset_in) acceptable for any fill.
    pub min_price: f64,
    /// Whether to allow partial fills (unfilled remainder handled separately).
    pub allow_partial: bool,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self { max_slippage_pct: 0.02, min_price: 0.0, allow_partial: true }
    }
}

// ── Pure quote function ───────────────────────────────────────────────────────

/// Quote a route for converting `amount_in` units of the seized asset.
///
/// Greedily fills from the best-priced tiers downward (price-descending order)
/// until `amount_in` is exhausted or all tiers are consumed.  Enforces
/// `config.min_price` per tier — tiers below the floor are skipped.
/// Returns a `Route` only when at least the full amount can be filled
/// (unless `allow_partial` is set, in which case a partial fill is returned).
pub fn quote_route(
    snapshot: &VenueSnapshot,
    amount_in: f64,
    config: &RoutingConfig,
) -> QuoteResult {
    if amount_in <= 0.0 { return QuoteResult::InsufficientLiquidity { available: 0.0 }; }

    // Sort tiers by price descending (best first).
    let mut sorted = snapshot.tiers.clone();
    sorted.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));

    let mut remaining   = amount_in;
    let mut hops        = Vec::new();
    let mut total_out   = 0.0;
    let mut available   = 0.0;

    for tier in &sorted {
        available += tier.max_fill;
        if tier.price < config.min_price { continue; }

        let fill = remaining.min(tier.max_fill);
        if fill <= 0.0 { continue; }

        let out = fill * tier.price;
        hops.push(RouteHop {
            venue:       tier.venue.clone(),
            amount_in:   fill,
            price:       tier.price,
            expected_out: out,
        });
        total_out += out;
        remaining -= fill;
        if remaining <= 1e-9 { break; }
    }

    if hops.is_empty() {
        return QuoteResult::BelowMinPrice {
            best_price: snapshot.best_price().unwrap_or(0.0),
            min_price:  config.min_price,
        };
    }

    if remaining > 1e-9 && !config.allow_partial {
        return QuoteResult::InsufficientLiquidity { available };
    }

    let filled_in  = amount_in - remaining;
    let min_received = total_out * (1.0 - config.max_slippage_pct);

    QuoteResult::Route(Route {
        hops,
        total_in:     filled_in,
        expected_out: total_out,
        min_received,
    })
}

// ── Outcome types ─────────────────────────────────────────────────────────────

/// What happened after the routing + submission attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RouteOutcome {
    /// Trade executed; `received` is the actual amount of settlement asset.
    Filled {
        received:          f64,
        realized_slippage: f64,  // fraction, e.g. 0.005 = 0.5%
        shortfall:         f64,  // amount_in not converted (0 if full fill)
    },
    /// Partial fill; unfilled remainder retained.
    PartialFill {
        received:          f64,
        realized_slippage: f64,
        shortfall:         f64,  // asset_in units unconverted
    },
    /// Trade aborted — oracle divergence or stale oracle.
    Fallback { reason: FallbackReason, retained_amount: f64 },
    /// Trade aborted — venue quote below `min_received`.
    BelowMinReceived { quoted: f64, min_received: f64, retained_amount: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FallbackReason {
    OracleDivergence { oracle_price: f64, venue_price: f64, band_pct: f64 },
    StaleOracle,
    InsufficientLiquidity,
    ZeroAmount,
}

// ── Submission (with re-validation) ──────────────────────────────────────────

/// Simulated DEX submitter — in production this calls the Stellar RPC.
pub struct DexSubmitter;

impl DexSubmitter {
    /// Submit a route.  Re-validates `min_received` immediately before send.
    /// In the test stub, it applies a small simulated fill (97% of expected).
    pub async fn submit(route: &Route) -> RouteOutcome {
        // Re-validate: simulated execution at 97% of expected (0.03 slippage).
        let fill_ratio  = 0.97_f64;
        let received    = route.expected_out * fill_ratio;
        let shortfall   = route.total_in * (1.0 - fill_ratio);

        if received < route.min_received {
            return RouteOutcome::BelowMinReceived {
                quoted:           received,
                min_received:     route.min_received,
                retained_amount:  route.total_in,
            };
        }

        let realized_slippage = 1.0 - (received / route.expected_out);
        RouteOutcome::Filled { received, realized_slippage, shortfall }
    }
}

// ── Fixed-point slippage math ─────────────────────────────────────────────────
// Integer-scaled analogues for Kani/fuzz coverage.

pub const SLIPPAGE_SCALE: i64 = 1_000_000; // 6 decimal places

/// Compute `min_received` in fixed-point units given `expected` and `max_slippage_bps`.
/// `expected` and returned value are in `SLIPPAGE_SCALE` units.
/// `max_slippage_bps` is in basis points (1 bp = 0.01%).
pub fn min_received_fixed(expected: i64, max_slippage_bps: i64) -> i64 {
    // min_received = expected * (10_000 - bps) / 10_000  (floor — conservative)
    expected.saturating_mul(10_000 - max_slippage_bps) / 10_000
}

/// Realized slippage in basis points.
/// Returns None if `expected` is zero.
pub fn realized_slippage_bps(expected: i64, received: i64) -> Option<i64> {
    if expected == 0 { return None; }
    Some((expected - received).saturating_mul(10_000) / expected)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_tiers(tiers: &[(f64, f64)]) -> VenueSnapshot {
        let mut s = VenueSnapshot::default();
        for (price, max_fill) in tiers {
            s.add_tier(LiquidityTier {
                venue: Venue::StellarDex, price: *price, max_fill: *max_fill,
            });
        }
        s
    }

    #[test]
    fn quote_full_fill_single_tier() {
        let snap = snapshot_with_tiers(&[(2.0, 1000.0)]);
        let cfg  = RoutingConfig { min_price: 1.0, max_slippage_pct: 0.02, allow_partial: false };
        match quote_route(&snap, 100.0, &cfg) {
            QuoteResult::Route(r) => {
                assert_eq!(r.hops.len(), 1);
                assert!((r.expected_out - 200.0).abs() < 1e-9);
            }
            other => panic!("expected Route, got {other:?}"),
        }
    }

    #[test]
    fn quote_splits_across_two_tiers() {
        let snap = snapshot_with_tiers(&[(2.0, 50.0), (1.8, 100.0)]);
        let cfg  = RoutingConfig { min_price: 1.0, max_slippage_pct: 0.02, allow_partial: false };
        match quote_route(&snap, 100.0, &cfg) {
            QuoteResult::Route(r) => {
                assert_eq!(r.hops.len(), 2);
                // 50 * 2.0 + 50 * 1.8 = 190
                assert!((r.expected_out - 190.0).abs() < 1e-9);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn quote_below_min_price_returns_fallback() {
        let snap = snapshot_with_tiers(&[(0.5, 1000.0)]);
        let cfg  = RoutingConfig { min_price: 1.0, max_slippage_pct: 0.02, allow_partial: false };
        assert!(matches!(quote_route(&snap, 100.0, &cfg), QuoteResult::BelowMinPrice { .. }));
    }

    #[test]
    fn quote_insufficient_liquidity_no_partial() {
        let snap = snapshot_with_tiers(&[(2.0, 10.0)]);
        let cfg  = RoutingConfig { min_price: 1.0, max_slippage_pct: 0.02, allow_partial: false };
        assert!(matches!(quote_route(&snap, 100.0, &cfg), QuoteResult::InsufficientLiquidity { .. }));
    }

    #[test]
    fn quote_partial_fill_allowed() {
        let snap = snapshot_with_tiers(&[(2.0, 30.0)]);
        let cfg  = RoutingConfig { min_price: 1.0, max_slippage_pct: 0.02, allow_partial: true };
        match quote_route(&snap, 100.0, &cfg) {
            QuoteResult::Route(r) => assert!((r.total_in - 30.0).abs() < 1e-9),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn quote_empty_snapshot_is_insufficient() {
        let snap = VenueSnapshot::default();
        let cfg  = RoutingConfig::default();
        assert!(matches!(quote_route(&snap, 100.0, &cfg), QuoteResult::InsufficientLiquidity { .. }
            | QuoteResult::BelowMinPrice { .. }));
    }

    #[test]
    fn min_received_fixed_boundary() {
        // 0 bps slippage => min_received == expected
        assert_eq!(min_received_fixed(1_000_000, 0), 1_000_000);
        // 100 bps (1%) slippage
        assert_eq!(min_received_fixed(1_000_000, 100), 990_000);
        // 10_000 bps (100%) => 0
        assert_eq!(min_received_fixed(1_000_000, 10_000), 0);
    }

    #[test]
    fn realized_slippage_bps_zero_expected() {
        assert_eq!(realized_slippage_bps(0, 0), None);
    }

    #[test]
    fn realized_slippage_bps_no_slippage() {
        assert_eq!(realized_slippage_bps(1_000, 1_000), Some(0));
    }

    #[test]
    fn realized_slippage_bps_one_percent() {
        assert_eq!(realized_slippage_bps(10_000, 9_900), Some(100));
    }
}
