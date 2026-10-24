//! Observable market-price ingestion and multi-source aggregation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod external;
pub mod sdex;
pub mod soroban_pool;

pub use external::ExternalVenueSource;
pub use sdex::{AssetId, SdexOrderBookSource};
pub use soroban_pool::SorobanPoolSource;

pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    pub price: f64,
    pub source: String,
    pub observed_at: DateTime<Utc>,
}

impl PriceTick {
    pub fn now(price: f64, source: impl Into<String>) -> Self {
        Self {
            price,
            source: source.into(),
            observed_at: Utc::now(),
        }
    }
}

#[derive(Debug)]
pub enum PriceError {
    RateLimited,
    Malformed(String),
    Timeout,
    Transport(String),
    Other(String),
    InsufficientSources { available: usize, required: usize },
}

impl std::fmt::Display for PriceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => f.write_str("price source rate limited"),
            Self::Malformed(detail) => write!(f, "malformed price response: {detail}"),
            Self::Timeout => f.write_str("price fetch timed out"),
            Self::Transport(detail) => write!(f, "price source transport error: {detail}"),
            Self::Other(detail) => write!(f, "price source error: {detail}"),
            Self::InsufficientSources {
                available,
                required,
            } => write!(
                f,
                "only {available} valid source(s) returned prices; {required} required"
            ),
        }
    }
}

impl std::error::Error for PriceError {}

#[async_trait::async_trait]
pub trait PriceSource: Send + Sync {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError>;

    fn name(&self) -> &'static str;
}

/// Combines independently observed quotes using a median, preventing a single
/// venue outlier from setting the published price.
pub struct AggregatedPriceSource {
    sources: Vec<Arc<dyn PriceSource>>,
    minimum_sources: usize,
}

impl AggregatedPriceSource {
    pub fn new(sources: Vec<Arc<dyn PriceSource>>, minimum_sources: usize) -> Self {
        Self {
            sources,
            minimum_sources: minimum_sources.max(1),
        }
    }
}

#[async_trait::async_trait]
impl PriceSource for AggregatedPriceSource {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError> {
        let mut observations: HashMap<String, Vec<PriceTick>> = HashMap::new();
        let mut source_errors = Vec::new();
        for source in &self.sources {
            match tokio::time::timeout(FETCH_TIMEOUT, source.fetch(symbols)).await {
                Ok(Ok(quotes)) => {
                    for (symbol, quote) in quotes {
                        if quote.price.is_finite() && quote.price > 0.0 {
                            observations.entry(symbol).or_default().push(quote);
                        }
                    }
                }
                Ok(Err(error)) => source_errors.push(format!("{}: {error}", source.name())),
                Err(_) => source_errors.push(format!("{}: timed out", source.name())),
            }
        }

        let available = observations
            .values()
            .map(Vec::len)
            .max()
            .unwrap_or_default();
        let mut aggregate = HashMap::new();
        for (symbol, mut quotes) in observations {
            if quotes.len() < self.minimum_sources {
                continue;
            }
            quotes.sort_by(|left, right| left.price.total_cmp(&right.price));
            let median = if quotes.len() % 2 == 0 {
                let right = quotes.len() / 2;
                (quotes[right - 1].price + quotes[right].price) / 2.0
            } else {
                quotes[quotes.len() / 2].price
            };
            let sources = quotes
                .iter()
                .map(|quote| quote.source.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let observed_at = quotes
                .iter()
                .map(|quote| quote.observed_at)
                .min()
                .unwrap_or_else(Utc::now);
            aggregate.insert(
                symbol,
                PriceTick {
                    price: median,
                    source: format!("median:{sources}"),
                    observed_at,
                },
            );
        }

        if aggregate.is_empty() {
            return Err(if source_errors.is_empty() {
                PriceError::InsufficientSources {
                    available,
                    required: self.minimum_sources,
                }
            } else {
                PriceError::Other(source_errors.join("; "))
            });
        }
        Ok(aggregate)
    }

    fn name(&self) -> &'static str {
        "market_median"
    }
}

/// The market adapters are opt-in and require explicit asset/pool mappings.
/// No simulated or seeded price source is selected when configuration is
/// missing; the ingestion loop keeps its last verified tick instead.
#[derive(Clone, Default)]
pub struct PriceSourceConfig {
    pub horizon_url: String,
    pub sdex_assets: HashMap<String, (AssetId, AssetId)>,
    pub soroban_quote_url: Option<String>,
    pub soroban_pools: HashMap<String, String>,
    pub external_quote_url: Option<String>,
    pub external_symbols: HashMap<String, String>,
    pub minimum_sources: usize,
}

impl PriceSourceConfig {
    pub fn from_env() -> Result<Self, PriceError> {
        fn json_map<T: for<'de> Deserialize<'de>>(
            key: &str,
        ) -> Result<HashMap<String, T>, PriceError> {
            match std::env::var(key) {
                Ok(value) => serde_json::from_str(&value)
                    .map_err(|e| PriceError::Malformed(format!("{key}: {e}"))),
                Err(_) => Ok(HashMap::new()),
            }
        }

        let minimum_sources = std::env::var("PRICE_MINIMUM_SOURCES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|e| PriceError::Malformed(format!("PRICE_MINIMUM_SOURCES: {e}")))?
            .unwrap_or(2);

        Ok(Self {
            horizon_url: std::env::var("ZENITH_HORIZON_URL")
                .unwrap_or_else(|_| "https://horizon-testnet.stellar.org".to_string()),
            sdex_assets: json_map("PRICE_SDEX_ASSETS")?,
            soroban_quote_url: std::env::var("PRICE_SOROBAN_POOL_QUOTES_URL").ok(),
            soroban_pools: json_map("PRICE_SOROBAN_POOL_IDS")?,
            external_quote_url: std::env::var("PRICE_EXTERNAL_QUOTES_URL").ok(),
            external_symbols: json_map("PRICE_EXTERNAL_SYMBOLS")?,
            minimum_sources,
        })
    }

    pub fn build(&self) -> Arc<dyn PriceSource> {
        let mut sources: Vec<Arc<dyn PriceSource>> = Vec::new();
        if !self.sdex_assets.is_empty() {
            sources.push(Arc::new(SdexOrderBookSource::new(
                self.horizon_url.clone(),
                self.sdex_assets.clone(),
            )));
        }
        if let Some(url) = &self.soroban_quote_url {
            if !self.soroban_pools.is_empty() {
                sources.push(Arc::new(SorobanPoolSource::new(
                    url.clone(),
                    self.soroban_pools.clone(),
                )));
            }
        }
        if let Some(url) = &self.external_quote_url {
            if !self.external_symbols.is_empty() {
                sources.push(Arc::new(ExternalVenueSource::new(
                    url.clone(),
                    self.external_symbols.clone(),
                )));
            }
        }
        Arc::new(AggregatedPriceSource::new(
            sources,
            self.minimum_sources,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSource {
        name: &'static str,
        price: f64,
    }

    #[async_trait::async_trait]
    impl PriceSource for FixedSource {
        async fn fetch(
            &self,
            symbols: &[String],
        ) -> Result<HashMap<String, PriceTick>, PriceError> {
            Ok(symbols
                .iter()
                .map(|symbol| {
                    (
                        symbol.clone(),
                        PriceTick::now(self.price, self.name),
                    )
                })
                .collect())
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    #[tokio::test]
    async fn median_aggregates_market_quotes_and_rejects_bad_values() {
        let aggregator = AggregatedPriceSource::new(
            vec![
                Arc::new(FixedSource {
                    name: "sdex",
                    price: 99.0,
                }),
                Arc::new(FixedSource {
                    name: "amm",
                    price: 100.0,
                }),
                Arc::new(FixedSource {
                    name: "venue",
                    price: 1_000.0,
                }),
                Arc::new(FixedSource {
                    name: "bad",
                    price: f64::NAN,
                }),
            ],
            3,
        );
        let result = aggregator.fetch(&["XLM".to_string()]).await.unwrap();
        assert_eq!(result["XLM"].price, 100.0);
        assert_eq!(result["XLM"].source, "median:sdex,amm,venue");
    }

    #[tokio::test]
    async fn requires_multiple_observations_per_symbol() {
        let aggregator = AggregatedPriceSource::new(
            vec![Arc::new(FixedSource {
                name: "only",
                price: 1.0,
            })],
            2,
        );
        assert!(matches!(
            aggregator.fetch(&["XLM".into()]).await,
            Err(PriceError::InsufficientSources {
                available: 0,
                required: 2
            })
        ));
    }
}
