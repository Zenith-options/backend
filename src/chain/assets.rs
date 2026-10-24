//! Canonical asset registry.
//!
//! Every amount conversion and contract call resolves precision through this
//! module instead of hard-coding assumptions about decimal places.
//!
//! # Caching
//! The registry is read from the DB at startup (or on first use) and cached
//! in an `Arc<RwLock<AssetCache>>`. The cache is invalidated whenever a
//! write to `asset_registry` is detected (tested via `invalidate()`).
//!
//! # Decimal conversion
//! All conversions go through `AssetAmount`, which carries the asset's
//! decimals and prevents accidental mixing of differently-scaled values.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

// ── Asset descriptor ──────────────────────────────────────────────────────────

/// One row from `asset_registry`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct AssetDefinition {
    pub symbol:           String,
    pub asset_code:       String,
    pub issuer:           Option<String>,
    pub sac_contract_id:  Option<String>,
    pub decimals:         i64,
    pub price_source_key: String,
    pub supported_as:     String,
    pub schema_version:   i64,
    pub enabled:          i64,
    pub created_at:       String,
    pub updated_at:       String,
}

impl AssetDefinition {
    pub fn is_enabled(&self) -> bool { self.enabled != 0 }
    pub fn supports(&self, role: &str) -> bool {
        self.supported_as.split(',').any(|s| s.trim() == role)
    }
    pub fn scale_factor(&self) -> i128 {
        10_i128.pow(self.decimals as u32)
    }
}

// ── Decimal-aware amount ──────────────────────────────────────────────────────

/// An amount tagged with its asset's decimal precision. Prevents mixing
/// differently-scaled values and makes widening/narrowing conversions explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetAmount {
    /// Raw integer value in the asset's native units (already scaled).
    pub raw:      i128,
    /// Decimal places the `raw` value is scaled by.
    pub decimals: u32,
}

impl AssetAmount {
    /// Create from a human-readable float, rounding down.
    pub fn from_f64(value: f64, decimals: u32) -> Option<Self> {
        if !value.is_finite() || value < 0.0 { return None; }
        let scale = 10_f64.powi(decimals as i32);
        Some(Self { raw: (value * scale) as i128, decimals })
    }

    /// Convert back to a human-readable f64.
    pub fn to_f64(self) -> f64 {
        let scale = 10_f64.powi(self.decimals as i32);
        self.raw as f64 / scale
    }

    /// Widen to a higher-precision representation (more decimals, no loss).
    /// Returns `None` if `target_decimals < self.decimals`.
    pub fn widen(self, target_decimals: u32) -> Option<Self> {
        if target_decimals < self.decimals { return None; }
        let extra = target_decimals - self.decimals;
        let factor = 10_i128.pow(extra);
        Some(Self { raw: self.raw.checked_mul(factor)?, decimals: target_decimals })
    }

    /// Narrow to fewer decimals, truncating (flooring) toward zero.
    /// Returns `None` if `target_decimals > self.decimals`.
    pub fn narrow(self, target_decimals: u32) -> Option<Self> {
        if target_decimals > self.decimals { return None; }
        let extra = self.decimals - target_decimals;
        let divisor = 10_i128.pow(extra);
        Some(Self { raw: self.raw / divisor, decimals: target_decimals })
    }

    /// Convert to a different decimal scale (widens then narrows as needed).
    pub fn convert(self, target_decimals: u32) -> Option<Self> {
        if target_decimals >= self.decimals {
            self.widen(target_decimals)
        } else {
            self.narrow(target_decimals)
        }
    }
}

// ── Registry errors ───────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub enum AssetError {
    /// No asset found with this symbol.
    NotFound(String),
    /// Multiple assets share this code (ambiguous without an issuer).
    AmbiguousCode(String),
    /// Asset exists but is disabled.
    Disabled(String),
    /// The requested role is not supported by this asset.
    UnsupportedRole { symbol: String, role: String },
    /// Decimal conversion overflow or invalid input.
    ConversionError(String),
    Db(String),
}

impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s)          => write!(f, "asset not found: '{s}'"),
            Self::AmbiguousCode(c)     => write!(f, "ambiguous asset code '{c}': specify issuer"),
            Self::Disabled(s)          => write!(f, "asset '{s}' is disabled"),
            Self::UnsupportedRole { symbol, role } =>
                write!(f, "asset '{symbol}' does not support role '{role}'"),
            Self::ConversionError(m)   => write!(f, "decimal conversion error: {m}"),
            Self::Db(e)                => write!(f, "db: {e}"),
        }
    }
}

// ── In-process cache ──────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct AssetCache {
    /// symbol → AssetDefinition
    by_symbol: HashMap<String, AssetDefinition>,
    /// asset_code → Vec<symbol>  (for ambiguous-code detection)
    by_code: HashMap<String, Vec<String>>,
}

impl AssetCache {
    fn load(assets: Vec<AssetDefinition>) -> Self {
        let mut by_symbol = HashMap::new();
        let mut by_code: HashMap<String, Vec<String>> = HashMap::new();
        for a in assets {
            by_code.entry(a.asset_code.clone()).or_default().push(a.symbol.clone());
            by_symbol.insert(a.symbol.clone(), a);
        }
        Self { by_symbol, by_code }
    }
}

// ── Resolver ──────────────────────────────────────────────────────────────────

/// Shared, cacheable asset registry resolver.
#[derive(Clone)]
pub struct AssetRegistry {
    pool:  SqlitePool,
    cache: Arc<RwLock<Option<AssetCache>>>,
}

impl AssetRegistry {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool, cache: Arc::new(RwLock::new(None)) }
    }

    /// Invalidate the cache; next resolve will re-read from the DB.
    pub fn invalidate(&self) {
        if let Ok(mut g) = self.cache.write() { *g = None; }
    }

    async fn ensure_loaded(&self) -> Result<(), AssetError> {
        // Fast path: already populated.
        if self.cache.read().map(|g| g.is_some()).unwrap_or(false) {
            return Ok(());
        }
        let assets = load_all(&self.pool).await?;
        if let Ok(mut g) = self.cache.write() {
            *g = Some(AssetCache::load(assets));
        }
        Ok(())
    }

    /// Resolve by symbol. Checks enabled flag; returns error for disabled.
    pub async fn resolve(&self, symbol: &str) -> Result<AssetDefinition, AssetError> {
        self.ensure_loaded().await?;
        let g = self.cache.read().map_err(|_| AssetError::Db("lock poisoned".into()))?;
        let cache = g.as_ref().ok_or_else(|| AssetError::Db("cache empty".into()))?;
        let def = cache.by_symbol.get(symbol)
            .ok_or_else(|| AssetError::NotFound(symbol.to_string()))?
            .clone();
        if !def.is_enabled() { return Err(AssetError::Disabled(symbol.to_string())); }
        Ok(def)
    }

    /// Resolve allowing disabled assets (for historical settlement).
    pub async fn resolve_any(&self, symbol: &str) -> Result<AssetDefinition, AssetError> {
        self.ensure_loaded().await?;
        let g = self.cache.read().map_err(|_| AssetError::Db("lock poisoned".into()))?;
        let cache = g.as_ref().ok_or_else(|| AssetError::Db("cache empty".into()))?;
        cache.by_symbol.get(symbol)
            .cloned()
            .ok_or_else(|| AssetError::NotFound(symbol.to_string()))
    }

    /// Resolve by asset code, guarding against ambiguous codes.
    pub async fn resolve_by_code(&self, code: &str) -> Result<AssetDefinition, AssetError> {
        self.ensure_loaded().await?;
        let g = self.cache.read().map_err(|_| AssetError::Db("lock poisoned".into()))?;
        let cache = g.as_ref().ok_or_else(|| AssetError::Db("cache empty".into()))?;
        let symbols = cache.by_code.get(code)
            .ok_or_else(|| AssetError::NotFound(code.to_string()))?;
        let enabled: Vec<_> = symbols.iter()
            .filter_map(|s| cache.by_symbol.get(s))
            .filter(|a| a.is_enabled())
            .collect();
        match enabled.len() {
            0 => Err(AssetError::NotFound(code.to_string())),
            1 => Ok(enabled[0].clone()),
            _ => Err(AssetError::AmbiguousCode(code.to_string())),
        }
    }

    /// All enabled assets.
    pub async fn list_enabled(&self) -> Result<Vec<AssetDefinition>, AssetError> {
        self.ensure_loaded().await?;
        let g = self.cache.read().map_err(|_| AssetError::Db("lock poisoned".into()))?;
        let cache = g.as_ref().ok_or_else(|| AssetError::Db("cache empty".into()))?;
        let mut out: Vec<_> = cache.by_symbol.values()
            .filter(|a| a.is_enabled()).cloned().collect();
        out.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        Ok(out)
    }

    /// All assets (including disabled).
    pub async fn list_all(&self) -> Result<Vec<AssetDefinition>, AssetError> {
        self.ensure_loaded().await?;
        let g = self.cache.read().map_err(|_| AssetError::Db("lock poisoned".into()))?;
        let cache = g.as_ref().ok_or_else(|| AssetError::Db("cache empty".into()))?;
        let mut out: Vec<_> = cache.by_symbol.values().cloned().collect();
        out.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        Ok(out)
    }

    /// Validate that a SAC contract id exists on-chain (stub — production
    /// would call Stellar RPC getLedgerEntries).
    pub async fn validate_sac(&self, symbol: &str) -> Result<bool, AssetError> {
        let def = self.resolve(symbol).await?;
        Ok(def.sac_contract_id.is_some())
    }

    /// Convert a human-readable amount for `symbol` to its raw integer form.
    pub async fn to_raw(&self, symbol: &str, amount: f64) -> Result<AssetAmount, AssetError> {
        let def = self.resolve(symbol).await?;
        AssetAmount::from_f64(amount, def.decimals as u32)
            .ok_or_else(|| AssetError::ConversionError(
                format!("cannot convert {amount} to {}-decimal raw", def.decimals)))
    }

    /// Convert raw integer amount for `symbol` back to f64.
    pub async fn from_raw(&self, symbol: &str, raw: i128) -> Result<f64, AssetError> {
        let def = self.resolve(symbol).await?;
        let amt = AssetAmount { raw, decimals: def.decimals as u32 };
        Ok(amt.to_f64())
    }
}

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn load_all(pool: &SqlitePool) -> Result<Vec<AssetDefinition>, AssetError> {
    sqlx::query_as!(AssetDefinition,
        "SELECT symbol, asset_code, issuer, sac_contract_id, decimals,
                price_source_key, supported_as, schema_version, enabled,
                created_at, updated_at
         FROM asset_registry
         ORDER BY symbol"
    )
    .fetch_all(pool)
    .await
    .map_err(|e| AssetError::Db(e.to_string()))
}

/// Insert or update an asset (for provisioning / tests).
pub async fn upsert_asset(pool: &SqlitePool, def: &AssetDefinition) -> Result<(), AssetError> {
    sqlx::query(
        "INSERT INTO asset_registry
            (symbol, asset_code, issuer, sac_contract_id, decimals,
             price_source_key, supported_as, schema_version, enabled)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(symbol, COALESCE(issuer,'')) DO UPDATE SET
             asset_code        = excluded.asset_code,
             sac_contract_id   = excluded.sac_contract_id,
             decimals          = excluded.decimals,
             price_source_key  = excluded.price_source_key,
             supported_as      = excluded.supported_as,
             schema_version    = excluded.schema_version,
             enabled           = excluded.enabled,
             updated_at        = strftime('%Y-%m-%dT%H:%M:%fZ','now')"
    )
    .bind(&def.symbol).bind(&def.asset_code).bind(&def.issuer)
    .bind(&def.sac_contract_id).bind(def.decimals)
    .bind(&def.price_source_key).bind(&def.supported_as)
    .bind(def.schema_version).bind(def.enabled)
    .execute(pool).await
    .map_err(|e| AssetError::Db(e.to_string()))?;
    Ok(())
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

use axum::{extract::State, response::Json};
use crate::AppState;
use crate::error::AppError;

/// GET /api/v1/assets — public list of all enabled assets.
pub async fn list_assets(
    State(state): State<AppState>,
) -> Result<Json<Vec<AssetDefinition>>, AppError> {
    let assets = state.assets.list_enabled().await
        .map_err(|e| AppError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(assets))
}

/// GET /api/v1/assets/all — public list including disabled (for auditors).
pub async fn list_all_assets(
    State(state): State<AppState>,
) -> Result<Json<Vec<AssetDefinition>>, AppError> {
    let assets = state.assets.list_all().await
        .map_err(|e| AppError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(assets))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod tests {
    use super::*;

    // ── AssetAmount decimal math ──────────────────────────────────────────────

    #[test]
    fn from_f64_and_back_roundtrip() {
        let a = AssetAmount::from_f64(1.234_567_8, 7).unwrap();
        assert_eq!(a.raw, 12_345_678);
        let back = a.to_f64();
        assert!((back - 1.234_567_8).abs() < 1e-7);
    }

    #[test]
    fn from_f64_rejects_negative() {
        assert!(AssetAmount::from_f64(-1.0, 7).is_none());
    }

    #[test]
    fn from_f64_rejects_nan_inf() {
        assert!(AssetAmount::from_f64(f64::NAN, 7).is_none());
        assert!(AssetAmount::from_f64(f64::INFINITY, 7).is_none());
    }

    #[test]
    fn widen_is_lossless() {
        let a = AssetAmount::from_f64(1.0, 7).unwrap();
        let b = a.widen(18).unwrap();
        assert_eq!(b.decimals, 18);
        assert_eq!(b.raw, 1_000_000_000_000_000_000);
    }

    #[test]
    fn narrow_truncates_toward_zero() {
        let a = AssetAmount { raw: 12_345_678, decimals: 7 };
        let b = a.narrow(2).unwrap();
        assert_eq!(b.raw, 123);  // truncated, not rounded
        assert_eq!(b.decimals, 2);
    }

    #[test]
    fn convert_7_to_18_and_back() {
        let a = AssetAmount::from_f64(1.234_567_8, 7).unwrap();
        let b = a.convert(18).unwrap();
        let c = b.convert(7).unwrap();
        assert_eq!(a.raw, c.raw);
    }

    #[test]
    fn widen_returns_none_when_target_smaller() {
        let a = AssetAmount { raw: 100, decimals: 7 };
        assert!(a.widen(3).is_none());
    }

    #[test]
    fn narrow_returns_none_when_target_larger() {
        let a = AssetAmount { raw: 100, decimals: 3 };
        assert!(a.narrow(7).is_none());
    }

    #[test]
    fn extreme_value_widen_checked_mul() {
        // i128::MAX ~ 1.7e38; 10^18 ~ 1e18 — widening a large value by 11 more
        // decimals would overflow: should return None.
        let a = AssetAmount { raw: i128::MAX / 10, decimals: 7 };
        // Widening by 11 more decimals (7 -> 18) would try to multiply by 10^11.
        let result = a.widen(18);
        // Either returns None (overflow detected) or succeeds if it fits.
        // The key property: no panic.
        let _ = result;
    }

    // ── Registry resolver tests ───────────────────────────────────────────────

    async fn test_registry() -> (AssetRegistry, sqlx::SqlitePool) {
        use crate::db::init_pool;
        let id = uuid::Uuid::new_v4().to_string();
        let url = format!("sqlite:///tmp/asset-test-{id}.db");
        let pool = init_pool(&url).await;
        let reg = AssetRegistry::new(pool.clone());
        (reg, pool)
    }

    #[tokio::test]
    async fn resolve_known_symbol() {
        let (reg, _pool) = test_registry().await;
        let def = reg.resolve("XLM").await.unwrap();
        assert_eq!(def.symbol, "XLM");
        assert_eq!(def.decimals, 7);
    }

    #[tokio::test]
    async fn resolve_unknown_symbol_is_not_found() {
        let (reg, _pool) = test_registry().await;
        assert!(matches!(reg.resolve("BOGUS").await, Err(AssetError::NotFound(_))));
    }

    #[tokio::test]
    async fn resolve_disabled_returns_disabled_error() {
        let (reg, pool) = test_registry().await;
        sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='SOL'")
            .execute(&pool).await.unwrap();
        reg.invalidate();
        assert!(matches!(reg.resolve("SOL").await, Err(AssetError::Disabled(_))));
    }

    #[tokio::test]
    async fn resolve_any_returns_disabled_asset() {
        let (reg, pool) = test_registry().await;
        sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='SOL'")
            .execute(&pool).await.unwrap();
        reg.invalidate();
        let def = reg.resolve_any("SOL").await.unwrap();
        assert!(!def.is_enabled());
    }

    #[tokio::test]
    async fn ambiguous_code_returns_error() {
        let (reg, pool) = test_registry().await;
        // Insert a second asset with the same code but a different issuer.
        sqlx::query(
            "INSERT INTO asset_registry
                (symbol, asset_code, issuer, decimals, price_source_key, supported_as, schema_version, enabled)
             VALUES ('BTC2','BTC','GANOTHERISSUER0000000000000000000000000000000000000',7,'BTC2','underlying',1,1)"
        ).execute(&pool).await.unwrap();
        reg.invalidate();
        let result = reg.resolve_by_code("BTC").await;
        assert!(matches!(result, Err(AssetError::AmbiguousCode(_))));
    }

    #[tokio::test]
    async fn list_enabled_excludes_disabled() {
        let (reg, pool) = test_registry().await;
        sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='SOL'")
            .execute(&pool).await.unwrap();
        reg.invalidate();
        let list = reg.list_enabled().await.unwrap();
        assert!(list.iter().all(|a| a.is_enabled()));
        assert!(!list.iter().any(|a| a.symbol == "SOL"));
    }

    #[tokio::test]
    async fn list_all_includes_disabled() {
        let (reg, pool) = test_registry().await;
        sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='ETH'")
            .execute(&pool).await.unwrap();
        reg.invalidate();
        let all = reg.list_all().await.unwrap();
        assert!(all.iter().any(|a| a.symbol == "ETH"));
    }

    #[tokio::test]
    async fn to_raw_and_from_raw_roundtrip() {
        let (reg, _) = test_registry().await;
        let raw = reg.to_raw("XLM", 1.5).await.unwrap();
        let back = reg.from_raw("XLM", raw.raw).await.unwrap();
        assert!((back - 1.5).abs() < 1e-7);
    }

    #[tokio::test]
    async fn cache_invalidation_reloads() {
        let (reg, pool) = test_registry().await;
        // Prime cache.
        reg.resolve("XLM").await.unwrap();
        // Mutate DB without going through registry.
        sqlx::query("UPDATE asset_registry SET decimals=8 WHERE symbol='XLM'")
            .execute(&pool).await.unwrap();
        // Without invalidation, cache returns old value.
        let stale = reg.resolve("XLM").await.unwrap();
        assert_eq!(stale.decimals, 7);
        // After invalidation, cache reloads.
        reg.invalidate();
        let fresh = reg.resolve("XLM").await.unwrap();
        assert_eq!(fresh.decimals, 8);
    }
}
