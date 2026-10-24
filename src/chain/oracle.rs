//! Oracle price reference — provides the "fair" price used as the divergence
//! guard before any DEX trade.  In production this would call a TWAP oracle
//! contract or a price-feed aggregator; for now it wraps the in-memory spot
//! prices already in AppState so the guard is always exercisable.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A single oracle price observation.
#[derive(Debug, Clone)]
pub struct OraclePrice {
    /// Asset identifier, e.g. "XLM", "USDC".
    pub asset: String,
    /// Price in terms of the settlement asset (e.g. USDC per unit).
    pub price: f64,
    /// Unix timestamp of the observation.
    pub timestamp_secs: u64,
}

/// How old an observation can be before it is considered stale.
pub const MAX_ORACLE_AGE_SECS: u64 = 300; // 5 minutes

impl OraclePrice {
    pub fn is_stale(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        now.saturating_sub(self.timestamp_secs) > MAX_ORACLE_AGE_SECS
    }
}

/// Oracle errors.
#[derive(Debug, PartialEq)]
pub enum OracleError {
    AssetNotFound(String),
    StalePrice { asset: String, age_secs: u64 },
}

impl std::fmt::Display for OracleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AssetNotFound(a) => write!(f, "oracle: asset '{a}' not found"),
            Self::StalePrice { asset, age_secs } =>
                write!(f, "oracle: price for '{asset}' is stale ({age_secs}s old)"),
        }
    }
}

/// Minimal oracle interface — allows mocking in tests.
pub trait Oracle: Send + Sync {
    fn price(&self, asset: &str) -> Result<OraclePrice, OracleError>;
}

/// In-memory oracle backed by the AppState spot-price map.
pub struct SpotOracle {
    prices: std::collections::HashMap<String, f64>,
    timestamp_secs: u64,
}

impl SpotOracle {
    pub fn new(prices: std::collections::HashMap<String, f64>) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        Self { prices, timestamp_secs: ts }
    }

    /// Build from AppState's spot_prices lock.
    pub fn from_app_state(state: &crate::AppState) -> Self {
        let prices = state.spot_prices.lock().unwrap().clone();
        Self::new(prices)
    }
}

impl Oracle for SpotOracle {
    fn price(&self, asset: &str) -> Result<OraclePrice, OracleError> {
        let price = self.prices.get(asset)
            .copied()
            .ok_or_else(|| OracleError::AssetNotFound(asset.to_string()))?;
        Ok(OraclePrice {
            asset: asset.to_string(),
            price,
            timestamp_secs: self.timestamp_secs,
        })
    }
}

/// Fixed oracle for testing — always returns the given price, never stale.
#[cfg(any(test, feature = "test-utils"))]
pub struct FixedOracle(pub String, pub f64);

#[cfg(any(test, feature = "test-utils"))]
impl Oracle for FixedOracle {
    fn price(&self, asset: &str) -> Result<OraclePrice, OracleError> {
        if asset == self.0 {
            Ok(OraclePrice {
                asset: asset.to_string(),
                price: self.1,
                timestamp_secs: SystemTime::now()
                    .duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs(),
            })
        } else {
            Err(OracleError::AssetNotFound(asset.to_string()))
        }
    }
}

/// Stale oracle for testing.
#[cfg(any(test, feature = "test-utils"))]
pub struct StaleOracle(pub String, pub f64);

#[cfg(any(test, feature = "test-utils"))]
impl Oracle for StaleOracle {
    fn price(&self, asset: &str) -> Result<OraclePrice, OracleError> {
        Ok(OraclePrice {
            asset: asset.to_string(),
            price: self.1,
            timestamp_secs: 0, // epoch — always stale
        })
    }
}
