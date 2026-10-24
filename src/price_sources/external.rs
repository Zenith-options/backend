use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

use super::{PriceError, PriceSource, PriceTick};

#[derive(Clone)]
pub struct ExternalVenueSource {
    quote_url: String,
    symbols: HashMap<String, String>,
    http: reqwest::Client,
}

impl ExternalVenueSource {
    /// `quote_url` must return a ticker response with a string `price` field
    /// when queried with the venue's `symbol` parameter (for example Binance's
    /// public `/api/v3/ticker/price` endpoint).
    pub fn new(quote_url: String, symbols: HashMap<String, String>) -> Self {
        Self {
            quote_url,
            symbols,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("valid external quote HTTP client configuration"),
        }
    }
}

#[async_trait::async_trait]
impl PriceSource for ExternalVenueSource {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError> {
        let mut quotes = HashMap::new();
        for symbol in symbols {
            let Some(venue_symbol) = self.symbols.get(symbol) else {
                continue;
            };
            let response = self
                .http
                .get(&self.quote_url)
                .query(&[("symbol", venue_symbol)])
                .send()
                .await
                .map_err(|e| PriceError::Transport(e.to_string()))?;
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(PriceError::RateLimited);
            }
            if !response.status().is_success() {
                return Err(PriceError::Other(format!(
                    "external venue returned HTTP {}",
                    response.status()
                )));
            }
            let quote: VenueQuote = response
                .json()
                .await
                .map_err(|e| PriceError::Malformed(e.to_string()))?;
            let price = quote
                .price
                .parse::<f64>()
                .map_err(|e| PriceError::Malformed(format!("invalid venue price: {e}")))?;
            if !price.is_finite() || price <= 0.0 {
                return Err(PriceError::Malformed(
                    "external venue returned a non-positive price".into(),
                ));
            }
            quotes.insert(symbol.clone(), PriceTick::now(price, self.name()));
        }
        Ok(quotes)
    }

    fn name(&self) -> &'static str {
        "external_venue"
    }
}

#[derive(Deserialize)]
struct VenueQuote {
    price: String,
}
