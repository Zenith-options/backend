use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::{PriceError, PriceSource, PriceTick};

#[derive(Debug, Clone, Deserialize)]
pub struct AssetId {
    pub asset_type: String,
    pub asset_code: Option<String>,
    pub asset_issuer: Option<String>,
}

#[derive(Clone)]
pub struct SdexOrderBookSource {
    horizon_url: String,
    assets: HashMap<String, (AssetId, AssetId)>,
    http: reqwest::Client,
}

impl SdexOrderBookSource {
    pub fn new(horizon_url: String, assets: HashMap<String, (AssetId, AssetId)>) -> Self {
        Self {
            horizon_url: horizon_url.trim_end_matches('/').to_string(),
            assets,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("valid SDEX HTTP client configuration"),
        }
    }

    async fn order_book_mid(
        &self,
        symbol: &str,
        base: &AssetId,
        counter: &AssetId,
    ) -> Result<f64, PriceError> {
        let url = reqwest::Url::parse(&format!("{}/order_book", self.horizon_url))
            .map_err(|e| PriceError::Malformed(e.to_string()))?;
        let mut query = add_asset("base", base)?;
        query.extend(add_asset("counter", counter)?);
        let response = self
            .http
            .get(url)
            .query(&query)
            .send()
            .await
            .map_err(|e| PriceError::Transport(e.to_string()))?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(PriceError::RateLimited);
        }
        if !response.status().is_success() {
            return Err(PriceError::Other(format!(
                "Horizon order book returned HTTP {}",
                response.status()
            )));
        }
        let book: OrderBook = response
            .json()
            .await
            .map_err(|e| PriceError::Malformed(e.to_string()))?;
        let bid = book
            .bids
            .first()
            .and_then(OrderBookLevel::price)
            .ok_or_else(|| PriceError::Malformed(format!("no bids for {symbol}")))?;
        let ask = book
            .asks
            .first()
            .and_then(OrderBookLevel::price)
            .ok_or_else(|| PriceError::Malformed(format!("no asks for {symbol}")))?;
        let mid = (bid + ask) / 2.0;
        if !mid.is_finite() || mid <= 0.0 || bid > ask {
            return Err(PriceError::Malformed(format!(
                "invalid SDEX spread for {symbol}"
            )));
        }
        Ok(mid)
    }
}

#[async_trait::async_trait]
impl PriceSource for SdexOrderBookSource {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError> {
        let mut quotes = HashMap::new();
        let mut source_error = None;
        for symbol in symbols {
            if let Some((base, counter)) = self.assets.get(symbol) {
                match self.order_book_mid(symbol, base, counter).await {
                    Ok(price) => {
                        quotes.insert(symbol.clone(), PriceTick::now(price, self.name()));
                    }
                    Err(error) => source_error = Some(error),
                }
            }
        }
        if quotes.is_empty() {
            if let Some(error) = source_error {
                return Err(error);
            }
        }
        Ok(quotes)
    }

    fn name(&self) -> &'static str {
        "stellar_sdex"
    }
}

fn add_asset(
    prefix: &str,
    asset: &AssetId,
) -> Result<Vec<(String, String)>, PriceError> {
    let mut query = vec![(
        format!("{prefix}_asset_type"),
        asset.asset_type.clone(),
    )];
    if asset.asset_type != "native" {
        let code = asset.asset_code.as_deref().ok_or_else(|| {
            PriceError::Malformed(format!("{prefix} asset has no asset_code"))
        })?;
        let issuer = asset.asset_issuer.as_deref().ok_or_else(|| {
            PriceError::Malformed(format!("{prefix} asset has no asset_issuer"))
        })?;
        query.push((format!("{prefix}_asset_code"), code.to_string()));
        query.push((format!("{prefix}_asset_issuer"), issuer.to_string()));
    }
    Ok(query)
}

#[derive(Deserialize)]
struct OrderBook {
    bids: Vec<OrderBookLevel>,
    asks: Vec<OrderBookLevel>,
}

#[derive(Deserialize)]
struct OrderBookLevel {
    price: Option<String>,
    price_r: Option<RationalPrice>,
}

#[derive(Deserialize)]
struct RationalPrice {
    n: Value,
    d: Value,
}

impl OrderBookLevel {
    fn price(&self) -> Option<f64> {
        self.price
            .as_deref()
            .and_then(|price| price.parse().ok())
            .or_else(|| {
                self.price_r.as_ref().and_then(|price| {
                    let numerator = number(&price.n)?;
                    let denominator = number(&price.d)?;
                    (denominator != 0.0).then_some(numerator / denominator)
                })
            })
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}
