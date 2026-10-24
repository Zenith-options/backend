use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{PriceError, PriceSource, PriceTick};

#[derive(Clone)]
pub struct SorobanPoolSource {
    quote_adapter_url: String,
    pools: HashMap<String, String>,
    http: reqwest::Client,
}

impl SorobanPoolSource {
    /// Connects to a read-only quote adapter backed by Soroban RPC. The
    /// adapter exposes `GET /pools/{pool_id}/quote?symbol=...` and returns the
    /// pool's reserve-derived price plus its ledger and observation timestamp.
    /// This keeps contract-specific pool storage decoding out of the backend
    /// until the deployment's AMM contract interface is configured.
    pub fn new(quote_adapter_url: String, pools: HashMap<String, String>) -> Self {
        Self {
            quote_adapter_url: quote_adapter_url.trim_end_matches('/').to_string(),
            pools,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("valid Soroban pool quote HTTP client configuration"),
        }
    }
}

#[async_trait::async_trait]
impl PriceSource for SorobanPoolSource {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError> {
        let mut quotes = HashMap::new();
        for symbol in symbols {
            let Some(pool_id) = self.pools.get(symbol) else {
                continue;
            };
            let mut url = reqwest::Url::parse(&self.quote_adapter_url)
                .map_err(|e| PriceError::Malformed(e.to_string()))?;
            url.path_segments_mut()
                .map_err(|_| PriceError::Malformed("invalid pool quote adapter URL".into()))?
                .extend(["pools", pool_id, "quote"]);
            let response = self
                .http
                .get(url)
                .query(&[("symbol", symbol)])
                .send()
                .await
                .map_err(|e| PriceError::Transport(e.to_string()))?;
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(PriceError::RateLimited);
            }
            if !response.status().is_success() {
                return Err(PriceError::Other(format!(
                    "Soroban pool quote adapter returned HTTP {}",
                    response.status()
                )));
            }
            let quote: PoolQuote = response
                .json()
                .await
                .map_err(|e| PriceError::Malformed(e.to_string()))?;
            let price = quote
                .price
                .parse::<f64>()
                .map_err(|e| PriceError::Malformed(format!("invalid pool price: {e}")))?;
            if !price.is_finite() || price <= 0.0 || quote.ledger == 0 {
                return Err(PriceError::Malformed(
                    "Soroban pool quote has an invalid price or ledger".into(),
                ));
            }
            let observed_at = DateTime::parse_from_rfc3339(&quote.observed_at)
                .map_err(|e| PriceError::Malformed(format!("invalid pool timestamp: {e}")))?
                .with_timezone(&Utc);
            quotes.insert(
                symbol.clone(),
                PriceTick {
                    price,
                    source: format!("{}:ledger-{}", self.name(), quote.ledger),
                    observed_at,
                },
            );
        }
        Ok(quotes)
    }

    fn name(&self) -> &'static str {
        "soroban_amm"
    }
}

#[derive(Deserialize)]
struct PoolQuote {
    price: String,
    ledger: u32,
    observed_at: String,
}
