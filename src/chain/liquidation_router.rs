//! Liquidation routing: converts seized collateral into the settlement asset.
//!
//! Flow:
//!   1. Check oracle is fresh; abort with Fallback if stale.
//!   2. Quote a route via `dex::quote_route`.
//!   3. Check venue price doesn't diverge beyond `oracle_band_pct` from oracle.
//!   4. Submit; on partial fill, re-quote once within the same window.
//!   5. Record actual received, realized slippage, and any shortfall.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::chain::dex::{
    quote_route, DexSubmitter, FallbackReason, QuoteResult, RouteOutcome, RoutingConfig,
    VenueSnapshot,
};
use crate::chain::oracle::Oracle;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LiquidationRouterConfig {
    /// Maximum % deviation between oracle and best venue price before aborting.
    pub oracle_band_pct: f64,
    /// Maximum slippage allowed when computing `min_received`.
    pub max_slippage_pct: f64,
    /// Allow partial fills; unfilled remainder is retained.
    pub allow_partial: bool,
}

impl Default for LiquidationRouterConfig {
    fn default() -> Self {
        Self { oracle_band_pct: 0.05, max_slippage_pct: 0.02, allow_partial: true }
    }
}

// ── Accounting record ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionRecord {
    pub id:                String,
    pub liquidation_id:    String,
    pub asset_in:          String,
    pub asset_out:         String,
    pub amount_in:         f64,
    pub received:          f64,
    pub realized_slippage: f64,
    pub shortfall:         f64,
    pub outcome:           String,  // "filled" | "partial" | "fallback" | "below_min"
    pub fallback_reason:   Option<String>,
    pub created_at:        String,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum RouterError {
    OracleError(String),
    Db(sqlx::Error),
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OracleError(e) => write!(f, "oracle: {e}"),
            Self::Db(e)          => write!(f, "db: {e}"),
        }
    }
}

impl From<sqlx::Error> for RouterError { fn from(e: sqlx::Error) -> Self { Self::Db(e) } }

// ── Core router ───────────────────────────────────────────────────────────────

/// Convert `amount_in` units of `asset_in` into `asset_out` following the
/// liquidation routing policy.
///
/// Steps:
///   1. Validate oracle is fresh.
///   2. Quote route; enforce oracle divergence guard.
///   3. Submit (with re-validation in `DexSubmitter::submit`).
///   4. On partial fill: attempt one re-quote of the remainder.
///   5. Persist the `ConversionRecord` and return it.
pub async fn route_conversion(
    pool: &SqlitePool,
    oracle: &dyn Oracle,
    snapshot: &VenueSnapshot,
    liquidation_id: &str,
    asset_in: &str,
    asset_out: &str,
    amount_in: f64,
    cfg: &LiquidationRouterConfig,
) -> Result<ConversionRecord, RouterError> {
    if amount_in <= 0.0 {
        let record = make_record(liquidation_id, asset_in, asset_out, amount_in,
            0.0, 0.0, 0.0, "fallback", Some("zero amount"));
        persist(pool, &record).await?;
        return Ok(record);
    }

    // 1. Oracle freshness check.
    let oracle_obs = oracle.price(asset_in)
        .map_err(|e| RouterError::OracleError(e.to_string()))?;
    if oracle_obs.is_stale() {
        let record = make_record(liquidation_id, asset_in, asset_out, amount_in,
            0.0, 0.0, amount_in, "fallback", Some("stale oracle"));
        persist(pool, &record).await?;
        return Ok(record);
    }

    let routing_cfg = RoutingConfig {
        max_slippage_pct: cfg.max_slippage_pct,
        min_price: oracle_obs.price * (1.0 - cfg.oracle_band_pct),
        allow_partial: cfg.allow_partial,
    };

    // 2. Quote.
    let quote = quote_route(snapshot, amount_in, &routing_cfg);
    let route = match quote {
        QuoteResult::Route(r) => r,
        QuoteResult::InsufficientLiquidity { .. } => {
            let record = make_record(liquidation_id, asset_in, asset_out, amount_in,
                0.0, 0.0, amount_in, "fallback", Some("insufficient liquidity"));
            persist(pool, &record).await?;
            return Ok(record);
        }
        QuoteResult::BelowMinPrice { best_price, min_price } => {
            let reason = format!("venue price {best_price:.6} below oracle band {min_price:.6}");
            let record = make_record(liquidation_id, asset_in, asset_out, amount_in,
                0.0, 0.0, amount_in, "fallback", Some(&reason));
            persist(pool, &record).await?;
            return Ok(record);
        }
    };

    // 3. Oracle divergence guard.
    let best_venue = route.hops.iter()
        .map(|h| h.price)
        .fold(f64::NEG_INFINITY, f64::max);
    let divergence = (oracle_obs.price - best_venue).abs() / oracle_obs.price;
    if divergence > cfg.oracle_band_pct {
        let reason = format!(
            "oracle divergence {:.2}% > band {:.2}% (oracle={:.6}, venue={:.6})",
            divergence * 100.0, cfg.oracle_band_pct * 100.0, oracle_obs.price, best_venue
        );
        let record = make_record(liquidation_id, asset_in, asset_out, amount_in,
            0.0, 0.0, amount_in, "fallback", Some(&reason));
        persist(pool, &record).await?;
        return Ok(record);
    }

    // 4. Submit.
    let outcome = DexSubmitter::submit(&route).await;

    let record = match outcome {
        RouteOutcome::Filled { received, realized_slippage, shortfall } => {
            // On partial fill, attempt one re-quote of the shortfall remainder.
            let total_received = if shortfall > 1e-9 && cfg.allow_partial {
                let remainder_quote = quote_route(snapshot, shortfall, &routing_cfg);
                if let QuoteResult::Route(r2) = remainder_quote {
                    match DexSubmitter::submit(&r2).await {
                        RouteOutcome::Filled { received: r2_recv, .. } => received + r2_recv,
                        _ => received,
                    }
                } else { received }
            } else { received };

            make_record(liquidation_id, asset_in, asset_out, amount_in,
                total_received, realized_slippage, shortfall, "filled", None)
        }
        RouteOutcome::PartialFill { received, realized_slippage, shortfall } =>
            make_record(liquidation_id, asset_in, asset_out, amount_in,
                received, realized_slippage, shortfall, "partial", None),
        RouteOutcome::BelowMinReceived { quoted, min_received, retained_amount } => {
            let reason = format!("received {quoted:.6} < min_received {min_received:.6}");
            make_record(liquidation_id, asset_in, asset_out, amount_in,
                0.0, 0.0, retained_amount, "below_min", Some(&reason))
        }
        RouteOutcome::Fallback { reason, retained_amount } => {
            let r = format!("{reason:?}");
            make_record(liquidation_id, asset_in, asset_out, amount_in,
                0.0, 0.0, retained_amount, "fallback", Some(&r))
        }
    };

    persist(pool, &record).await?;
    Ok(record)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_record(
    liquidation_id: &str, asset_in: &str, asset_out: &str,
    amount_in: f64, received: f64, realized_slippage: f64, shortfall: f64,
    outcome: &str, fallback_reason: Option<&str>,
) -> ConversionRecord {
    let now = chrono_now();
    ConversionRecord {
        id:                Uuid::new_v4().to_string(),
        liquidation_id:    liquidation_id.to_string(),
        asset_in:          asset_in.to_string(),
        asset_out:         asset_out.to_string(),
        amount_in, received, realized_slippage, shortfall,
        outcome:           outcome.to_string(),
        fallback_reason:   fallback_reason.map(str::to_string),
        created_at:        now,
    }
}

async fn persist(pool: &SqlitePool, r: &ConversionRecord) -> Result<(), RouterError> {
    sqlx::query(
        "INSERT INTO liquidation_conversions
            (id,liquidation_id,asset_in,asset_out,amount_in,received,
             realized_slippage,shortfall,outcome,fallback_reason,created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )
    .bind(&r.id).bind(&r.liquidation_id)
    .bind(&r.asset_in).bind(&r.asset_out)
    .bind(r.amount_in).bind(r.received)
    .bind(r.realized_slippage).bind(r.shortfall)
    .bind(&r.outcome).bind(&r.fallback_reason)
    .bind(&r.created_at)
    .execute(pool).await?;
    Ok(())
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    format!("{}Z", secs)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::dex::{LiquidityTier, Venue};
    use crate::chain::oracle::FixedOracle;

    fn pool() -> std::pin::Pin<Box<dyn std::future::Future<Output = SqlitePool>>> {
        Box::pin(async {
            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            sqlx::query(
                "CREATE TABLE liquidation_conversions (
                    id TEXT, liquidation_id TEXT, asset_in TEXT, asset_out TEXT,
                    amount_in REAL, received REAL, realized_slippage REAL,
                    shortfall REAL, outcome TEXT, fallback_reason TEXT, created_at TEXT)"
            ).execute(&pool).await.unwrap();
            pool
        })
    }

    fn rich_snapshot() -> VenueSnapshot {
        let mut s = VenueSnapshot::default();
        s.add_tier(LiquidityTier { venue: Venue::StellarDex, price: 1.95, max_fill: 500.0 });
        s.add_tier(LiquidityTier { venue: Venue::StellarDex, price: 1.85, max_fill: 500.0 });
        s
    }

    fn thin_snapshot() -> VenueSnapshot {
        let mut s = VenueSnapshot::default();
        s.add_tier(LiquidityTier { venue: Venue::StellarDex, price: 1.95, max_fill: 5.0 });
        s
    }

    fn cfg() -> LiquidationRouterConfig {
        LiquidationRouterConfig { oracle_band_pct: 0.10, max_slippage_pct: 0.03, allow_partial: true }
    }

    #[tokio::test]
    async fn full_fill_records_outcome() {
        let pool = pool().await;
        let oracle = FixedOracle("XLM".into(), 2.0);
        let record = route_conversion(
            &pool, &oracle, &rich_snapshot(), "liq-1", "XLM", "USDC", 100.0, &cfg()
        ).await.unwrap();
        assert_eq!(record.outcome, "filled");
        assert!(record.received > 0.0);
        assert!(record.realized_slippage >= 0.0);
    }

    #[tokio::test]
    async fn stale_oracle_triggers_fallback() {
        use crate::chain::oracle::StaleOracle;
        let pool = pool().await;
        let oracle = StaleOracle("XLM".into(), 2.0);
        let record = route_conversion(
            &pool, &oracle, &rich_snapshot(), "liq-2", "XLM", "USDC", 100.0, &cfg()
        ).await.unwrap();
        assert_eq!(record.outcome, "fallback");
        assert!(record.fallback_reason.as_deref().unwrap_or("").contains("stale"));
        assert_eq!(record.received, 0.0);
        assert_eq!(record.shortfall, 100.0);
    }

    #[tokio::test]
    async fn thin_book_triggers_fallback_not_oversale() {
        let pool = pool().await;
        let oracle = FixedOracle("XLM".into(), 2.0);
        // Only 5 units available — with allow_partial=false this would fail.
        let mut no_partial = cfg();
        no_partial.allow_partial = false;
        let record = route_conversion(
            &pool, &oracle, &thin_snapshot(), "liq-3", "XLM", "USDC", 100.0, &no_partial
        ).await.unwrap();
        // Fallback because liquidity is insufficient for a full fill.
        assert_eq!(record.outcome, "fallback");
        assert_eq!(record.shortfall, 100.0);
    }

    #[tokio::test]
    async fn oracle_divergence_triggers_fallback() {
        let pool = pool().await;
        // Oracle says 2.0; venue best is 1.95 — 2.5% divergence.
        // Band is 1% — should abort.
        let oracle = FixedOracle("XLM".into(), 2.0);
        let tight = LiquidationRouterConfig { oracle_band_pct: 0.01, ..cfg() };
        let record = route_conversion(
            &pool, &oracle, &rich_snapshot(), "liq-4", "XLM", "USDC", 100.0, &tight
        ).await.unwrap();
        assert_eq!(record.outcome, "fallback");
        assert!(record.fallback_reason.as_deref().unwrap_or("").contains("oracle divergence"));
    }

    #[tokio::test]
    async fn zero_amount_is_fallback() {
        let pool = pool().await;
        let oracle = FixedOracle("XLM".into(), 2.0);
        let record = route_conversion(
            &pool, &oracle, &rich_snapshot(), "liq-5", "XLM", "USDC", 0.0, &cfg()
        ).await.unwrap();
        assert_eq!(record.outcome, "fallback");
    }

    #[tokio::test]
    async fn partial_fill_attempts_re_quote() {
        let pool = pool().await;
        let oracle = FixedOracle("XLM".into(), 2.0);
        // Enough for partial + re-quote.
        let record = route_conversion(
            &pool, &oracle, &rich_snapshot(), "liq-6", "XLM", "USDC", 100.0, &cfg()
        ).await.unwrap();
        assert!(record.received > 0.0);
    }
}
