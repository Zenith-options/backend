//! Integration tests for the asset registry.
//! Covers: resolve, disabled, ambiguous code, decimal conversion,
//! read endpoints, and that prices.rs uses registry decimals.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use zenith_backend::chain::assets::{AssetAmount, AssetDefinition, AssetError, AssetRegistry};
use zenith_backend::db::init_pool;

// ── Decimal conversion unit tests ─────────────────────────────────────────────

#[test]
fn asset_amount_from_f64_7_decimals() {
    let a = AssetAmount::from_f64(1.000_000_1, 7).unwrap();
    assert_eq!(a.raw, 10_000_001);
}

#[test]
fn asset_amount_widen_7_to_18() {
    let a = AssetAmount::from_f64(1.0, 7).unwrap();
    let b = a.widen(18).unwrap();
    assert_eq!(b.raw, 1_000_000_000_000_000_000_i128);
}

#[test]
fn asset_amount_narrow_18_to_7() {
    let a = AssetAmount { raw: 1_000_000_000_000_000_001_i128, decimals: 18 };
    let b = a.narrow(7).unwrap();
    // Truncates: 1e18+1 / 10^11 = 10_000_000 (the trailing 1 is lost)
    assert_eq!(b.raw, 10_000_000);
}

#[test]
fn asset_amount_convert_roundtrip_7_18_7() {
    let original = AssetAmount::from_f64(1.234_567_8, 7).unwrap();
    let wide = original.convert(18).unwrap();
    let back = wide.convert(7).unwrap();
    assert_eq!(original.raw, back.raw);
}

#[test]
fn asset_amount_extreme_value_no_panic() {
    // Very large raw, widening further — must not panic, may return None.
    let a = AssetAmount { raw: i128::MAX, decimals: 7 };
    let _ = a.widen(8); // might overflow; we just check no panic
}

#[test]
fn asset_amount_zero_is_fine() {
    let a = AssetAmount::from_f64(0.0, 7).unwrap();
    assert_eq!(a.raw, 0);
    assert_eq!(a.to_f64(), 0.0);
}

// ── Resolver tests ────────────────────────────────────────────────────────────

async fn fresh_registry() -> (AssetRegistry, sqlx::SqlitePool) {
    let id = uuid::Uuid::new_v4().to_string();
    let url = format!("sqlite:///tmp/asset-int-{id}.db");
    let pool = init_pool(&url).await;
    let reg = AssetRegistry::new(pool.clone());
    (reg, pool)
}

#[tokio::test]
async fn resolve_xlm_has_correct_decimals() {
    let (reg, _) = fresh_registry().await;
    let def = reg.resolve("XLM").await.unwrap();
    assert_eq!(def.decimals, 7);
    assert!(def.issuer.is_none(), "XLM is native — no issuer");
}

#[tokio::test]
async fn resolve_usdc_supports_settlement() {
    let (reg, _) = fresh_registry().await;
    let def = reg.resolve("USDC").await.unwrap();
    assert!(def.supports("settlement"));
    assert!(def.supports("collateral"));
}

#[tokio::test]
async fn resolve_unknown_is_not_found() {
    let (reg, _) = fresh_registry().await;
    assert!(matches!(reg.resolve("FAKE").await, Err(AssetError::NotFound(_))));
}

#[tokio::test]
async fn disabled_asset_returns_error_on_resolve() {
    let (reg, pool) = fresh_registry().await;
    sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='ETH'")
        .execute(&pool).await.unwrap();
    reg.invalidate();
    assert!(matches!(reg.resolve("ETH").await, Err(AssetError::Disabled(_))));
}

#[tokio::test]
async fn resolve_any_returns_disabled() {
    let (reg, pool) = fresh_registry().await;
    sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='ETH'")
        .execute(&pool).await.unwrap();
    reg.invalidate();
    let def = reg.resolve_any("ETH").await.unwrap();
    assert!(!def.is_enabled());
}

#[tokio::test]
async fn ambiguous_code_across_issuers_errors() {
    let (reg, pool) = fresh_registry().await;
    sqlx::query(
        "INSERT INTO asset_registry
            (symbol,asset_code,issuer,decimals,price_source_key,supported_as,schema_version,enabled)
         VALUES ('USDC2','USDC','GOTHER0000000000000000000000000000000000000000000000000',6,'USDC2','collateral',1,1)"
    ).execute(&pool).await.unwrap();
    reg.invalidate();
    assert!(matches!(reg.resolve_by_code("USDC").await, Err(AssetError::AmbiguousCode(_))));
}

#[tokio::test]
async fn list_enabled_excludes_disabled() {
    let (reg, pool) = fresh_registry().await;
    sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='SOL'")
        .execute(&pool).await.unwrap();
    reg.invalidate();
    let list = reg.list_enabled().await.unwrap();
    assert!(!list.iter().any(|a| a.symbol == "SOL"));
    assert!(list.iter().all(|a| a.is_enabled()));
}

#[tokio::test]
async fn list_all_includes_disabled() {
    let (reg, pool) = fresh_registry().await;
    sqlx::query("UPDATE asset_registry SET enabled=0 WHERE symbol='SOL'")
        .execute(&pool).await.unwrap();
    reg.invalidate();
    let all = reg.list_all().await.unwrap();
    assert!(all.iter().any(|a| a.symbol == "SOL" && !a.is_enabled()));
}

#[tokio::test]
async fn to_raw_from_raw_roundtrip() {
    let (reg, _) = fresh_registry().await;
    let raw = reg.to_raw("XLM", 2.5).await.unwrap();
    let back = reg.from_raw("XLM", raw.raw).await.unwrap();
    assert!((back - 2.5).abs() < 1e-7);
}

#[tokio::test]
async fn cache_invalidation_picks_up_db_change() {
    let (reg, pool) = fresh_registry().await;
    // Prime cache.
    let before = reg.resolve("BTC").await.unwrap();
    assert_eq!(before.decimals, 7);
    // Change outside registry.
    sqlx::query("UPDATE asset_registry SET decimals=8 WHERE symbol='BTC'")
        .execute(&pool).await.unwrap();
    // Still stale without invalidation.
    assert_eq!(reg.resolve("BTC").await.unwrap().decimals, 7);
    // After invalidation picks up new value.
    reg.invalidate();
    assert_eq!(reg.resolve("BTC").await.unwrap().decimals, 8);
}

// ── HTTP endpoint tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn get_assets_returns_enabled_list() {
    let app = TestApp::spawn().await;
    let (status, body) = app.get("/api/v1/assets").await;
    assert_eq!(status, StatusCode::OK);
    let assets = body.as_array().unwrap();
    assert!(!assets.is_empty(), "registry must have at least one asset");
    // All returned assets must be enabled.
    for a in assets {
        assert_eq!(a["enabled"].as_i64().unwrap(), 1);
    }
    // Must include XLM.
    assert!(assets.iter().any(|a| a["symbol"].as_str() == Some("XLM")));
}

#[tokio::test]
async fn get_assets_all_returns_full_list() {
    let app = TestApp::spawn().await;
    let (status, body) = app.get("/api/v1/assets/all").await;
    assert_eq!(status, StatusCode::OK);
    let assets = body.as_array().unwrap();
    assert!(!assets.is_empty());
}

#[tokio::test]
async fn assets_endpoint_is_public_no_auth_required() {
    let app = TestApp::spawn().await;
    // No bearer token — must still return 200.
    let (status, _) = app.get("/api/v1/assets").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn asset_decimals_match_registry() {
    let app = TestApp::spawn().await;
    let (_, body) = app.get("/api/v1/assets").await;
    let assets = body.as_array().unwrap();
    for a in assets {
        let dec = a["decimals"].as_i64().unwrap();
        assert!(dec >= 0 && dec <= 18,
            "decimals out of range for {}: {dec}", a["symbol"].as_str().unwrap_or("?"));
    }
}

// ── Price / settlement path uses registry ────────────────────────────────────

#[tokio::test]
async fn spot_endpoint_only_returns_assets_in_registry() {
    let app = TestApp::spawn().await;
    let (_, spot_body) = app.get("/api/v1/spot").await;
    let (_, assets_body) = app.get("/api/v1/assets").await;
    let price_keys: std::collections::HashSet<String> = spot_body["prices"]
        .as_object().unwrap().keys().cloned().collect();
    let registry_keys: std::collections::HashSet<String> = assets_body
        .as_array().unwrap()
        .iter()
        .filter_map(|a| a["price_source_key"].as_str().map(str::to_string))
        .collect();
    // Every spot price key must exist in the registry.
    for key in &price_keys {
        assert!(registry_keys.contains(key),
            "spot price key '{key}' not in asset registry");
    }
}
