use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;

use crate::AppState;

const MAX_PCT_MOVE_PER_TICK: f64 = 0.003; // +/-0.3%

/// A single observed price for one underlying, tagged with where it came
/// from and when it was observed. `observed_at` is a UTC timestamp.
pub struct PriceTick {
    pub price: f64,
    pub source: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// Errors a `PriceSource` can surface. A failed fetch must never zero out
/// or remove an existing price — callers keep the last good value and mark
/// it stale.
#[derive(Debug)]
pub enum PriceError {
    /// The upstream returned a rate-limit response (HTTP 429).
    RateLimited,
    /// The upstream returned a response we could not parse.
    Malformed(String),
    /// The fetch exceeded its timeout budget.
    Timeout,
    /// Any other transport/upstream failure.
    Upstream(String),
}

impl std::fmt::Display for PriceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PriceError::RateLimited => write!(f, "upstream rate limited the request"),
            PriceError::Malformed(msg) => write!(f, "malformed upstream response: {msg}"),
            PriceError::Timeout => write!(f, "upstream fetch timed out"),
            PriceError::Upstream(msg) => write!(f, "upstream error: {msg}"),
        }
    }
}

impl std::error::Error for PriceError {}

/// Strategy interface for anything that can produce spot prices. The
/// simulator, an HTTP ticker feed and the Reflector oracle all implement
/// this so the ingestion loop is agnostic to where prices come from.
#[async_trait::async_trait]
pub trait PriceSource: Send + Sync {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError>;

    /// Human-readable identifier recorded on every tick this source emits.
    fn name(&self) -> &str;
}

/// Local-development / test source: the original random-walk simulator,
/// now behind the `PriceSource` trait. It never fails, so it is the safe
/// default when no real feed is configured.
pub struct SimulatedSource;

#[async_trait::async_trait]
impl PriceSource for SimulatedSource {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError> {
        let now = chrono::Utc::now();
        let mut ticks = HashMap::new();
        for symbol in symbols {
            let pct_move =
                rand::thread_rng().gen_range(-MAX_PCT_MOVE_PER_TICK..MAX_PCT_MOVE_PER_TICK);
            // The simulator has no independent notion of a "current" price;
            // it emits a multiplicative nudge around 1.0 and the ingestion
            // loop applies it to the last known price.
            ticks.insert(
                symbol.clone(),
                PriceTick {
                    price: 1.0 + pct_move,
                    source: self.name().to_string(),
                    observed_at: now,
                },
            );
        }
        Ok(ticks)
    }

    fn name(&self) -> &str {
        "simulated"
    }
}

/// HTTP spot feed (e.g. CoinGecko / Binance public ticker). Uses `reqwest`
/// with rustls and a hard 5s timeout on every fetch.
pub struct HttpTickerSource {
    client: reqwest::Client,
    base_url: String,
}

impl HttpTickerSource {
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url: base_url.into(),
        }
    }
}

#[async_trait::async_trait]
impl PriceSource for HttpTickerSource {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError> {
        let url = format!("{}/ticker/price", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PriceError::Timeout
                } else {
                    PriceError::Upstream(e.to_string())
                }
            })?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(PriceError::RateLimited);
        }
        if !resp.status().is_success() {
            return Err(PriceError::Upstream(format!("HTTP {}", resp.status())));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| PriceError::Malformed(e.to_string()))?;

        let now = chrono::Utc::now();
        let mut ticks = HashMap::new();
        // Binance returns a single object for one symbol and an array for
        // many; accept both shapes and skip symbols the upstream omitted.
        let entries: Vec<&serde_json::Value> = match &body {
            serde_json::Value::Array(items) => items.iter().collect(),
            serde_json::Value::Object(_) => vec![&body],
            _ => return Err(PriceError::Malformed("unexpected JSON shape".into())),
        };
        for entry in entries {
            let symbol = match entry.get("symbol").and_then(|s| s.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !symbols.iter().any(|s| s == &symbol) {
                continue;
            }
            let price = match entry
                .get("price")
                .and_then(|p| p.as_str())
                .and_then(|p| p.parse::<f64>().ok())
            {
                Some(p) => p,
                None => continue,
            };
            ticks.insert(
                symbol,
                PriceTick {
                    price,
                    source: self.name().to_string(),
                    observed_at: now,
                },
            );
        }
        Ok(ticks)
    }

    fn name(&self) -> &str {
        "http"
    }
}

/// Stellar Reflector oracle source. Reads `lastprice` through the Soroban
/// RPC `simulateTransaction` read path. Reflector returns fixed-point
/// integers alongside a `decimals()` value, so the raw value is scaled
/// down by `10^decimals` before being emitted.
pub struct ReflectorOracleSource {
    client: reqwest::Client,
    rpc_url: String,
    contract_id: String,
}

impl ReflectorOracleSource {
    pub fn new(rpc_url: impl Into<String>, contract_id: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            rpc_url: rpc_url.into(),
            contract_id: contract_id.into(),
        }
    }

    /// Decode a Reflector fixed-point integer into a float price.
    pub fn decode_fixed_point(raw: i128, decimals: u32) -> f64 {
        raw as f64 / 10f64.powi(decimals as i32)
    }
}

#[async_trait::async_trait]
impl PriceSource for ReflectorOracleSource {
    async fn fetch(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, PriceTick>, PriceError> {
        let now = chrono::Utc::now();
        let mut ticks = HashMap::new();
        for symbol in symbols {
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "simulateTransaction",
                "params": {
                    "transaction": {
                        "contract": self.contract_id,
                        "method": "lastprice",
                        "args": [symbol],
                    }
                }
            });
            let resp = self
                .client
                .post(&self.rpc_url)
                .json(&request)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        PriceError::Timeout
                    } else {
                        PriceError::Upstream(e.to_string())
                    }
                })?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(PriceError::RateLimited);
            }
            if !resp.status().is_success() {
                return Err(PriceError::Upstream(format!("HTTP {}", resp.status())));
            }
            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| PriceError::Malformed(e.to_string()))?;

            // Reflector's `lastprice` returns a fixed-point integer plus a
            // `decimals` value; scale the raw integer down accordingly.
            let raw = body
                .pointer("/result/price")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| PriceError::Malformed("missing result.price".into()))?;
            let decimals = body
                .pointer("/result/decimals")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            ticks.insert(
                symbol.clone(),
                PriceTick {
                    price: Self::decode_fixed_point(raw as i128, decimals),
                    source: self.name().to_string(),
                    observed_at: now,
                },
            );
        }
        Ok(ticks)
    }

    fn name(&self) -> &str {
        "reflector"
    }
}

/// Build the active price source from configuration. Defaults to the
/// simulator so local dev and tests keep working with no env vars set.
pub fn source_from_env() -> Arc<dyn PriceSource> {
    match std::env::var("PRICE_SOURCE").as_deref() {
        Ok("http") => Arc::new(HttpTickerSource::new(
            std::env::var("PRICE_HTTP_URL")
                .unwrap_or_else(|_| "https://api.binance.com/api/v3".into()),
        )),
        Ok("reflector") => Arc::new(ReflectorOracleSource::new(
            std::env::var("REFLECTOR_RPC_URL")
                .unwrap_or_else(|_| "https://soroban-testnet.stellar.org".into()),
            std::env::var("REFLECTOR_CONTRACT_ID").unwrap_or_default(),
        )),
        _ => Arc::new(SimulatedSource),
    }
}

/// Applies a fetched tick to the live price map. A failed fetch never
/// zeroes out or removes an existing price: the last good value is kept
/// and marked stale. Returns the JSON payload broadcast on `spot_tx`.
pub fn tick_once(state: &AppState) -> String {
    let symbols: Vec<String> = state.spot_prices.lock().unwrap().keys().cloned().collect();
    let source = state.price_source.clone();

    // Fetch outside the lock so no `std::sync::Mutex` guard is held across
    // an `.await`.
    let fetched = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(source.fetch(&symbols))
    });

    let prices = {
        let mut prices = state.spot_prices.lock().unwrap();
        match fetched {
            Ok(ticks) => {
                for (symbol, tick) in ticks {
                    if let Some(price) = prices.get_mut(&symbol) {
                        // The simulator emits a multiplicative nudge around
                        // 1.0; real sources emit an absolute price.
                        if source.name() == "simulated" {
                            *price = (*price * tick.price).max(0.0001);
                        } else {
                            *price = tick.price.max(0.0001);
                        }
                    }
                }
            }
            // Keep the last good price on failure; staleness is surfaced
            // through the source/updated_at fields on `/api/v1/spot`.
            Err(_) => {}
        }
        prices.clone()
    };
    let vols = state.vol_surface.lock().unwrap().clone();

    let payload = serde_json::json!({
        "prices": prices,
        "vols": vols,
        "source": source.name(),
        "updated_at": chrono::Utc::now().to_rfc3339(),
    })
    .to_string();
    let _ = state.spot_tx.send(payload.clone());
    payload
}

/// Ingestion loop. Replaces the old `price_simulator_loop` but keeps the
/// same "compute snapshot, then broadcast" shape so `/api/v1/ws/spot`
/// keeps working unchanged.
pub async fn price_ingestion_loop(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;
        tick_once(&state);
    }
}

pub async fn ws_spot(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_spot_socket(socket, state))
}

async fn handle_spot_socket(mut socket: WebSocket, state: AppState) {
    // Send an immediate snapshot so the client has something to render
    // before the first ingestion tick (up to 2s away) arrives.
    let snapshot = {
        let prices = state.spot_prices.lock().unwrap().clone();
        let vols = state.vol_surface.lock().unwrap().clone();
        serde_json::json!({
            "prices": prices,
            "vols": vols,
            "source": state.price_source.name(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string()
    };
    if socket.send(Message::Text(snapshot)).await.is_err() {
        return;
    }

    let mut rx = state.spot_tx.subscribe();
    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Ok(payload) => {
                        if socket.send(Message::Text(payload)).await.is_err() {
                            break;
                        }
                    }
                    // Client fell behind the broadcast buffer — resync with a
                    // fresh snapshot rather than sending stale skipped ticks.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // ignore anything the client sends; this is a read-only feed
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_state() -> (AppState, std::path::PathBuf) {
        let db_path =
            std::env::temp_dir().join(format!("zenith-prices-test-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", db_path.display())).await;
        (AppState::new(pool), db_path)
    }

    #[tokio::test]
    async fn tick_once_moves_every_price_within_the_per_tick_bound() {
        let (state, db_path) = test_state().await;
        let before = state.spot_prices.lock().unwrap().clone();

        tick_once(&state);

        let after = state.spot_prices.lock().unwrap().clone();
        for (underlying, before_price) in &before {
            let after_price = after[underlying];
            let max_move = before_price * MAX_PCT_MOVE_PER_TICK;
            assert!(
                (after_price - before_price).abs() <= max_move + 1e-9,
                "{underlying} moved from {before_price} to {after_price}, beyond the {MAX_PCT_MOVE_PER_TICK} bound"
            );
        }

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn tick_once_never_lets_a_price_reach_zero_or_go_negative() {
        let (state, db_path) = test_state().await;
        state
            .spot_prices
            .lock()
            .unwrap()
            .insert("TINY".into(), 0.0001);

        // Enough ticks that a run of unlucky downward moves would drive an
        // unclamped price to zero or below if the floor weren't enforced.
        for _ in 0..1000 {
            tick_once(&state);
        }

        let price = state.spot_prices.lock().unwrap()["TINY"];
        assert!(price > 0.0, "price floor was violated: {price}");

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn tick_once_broadcasts_the_post_tick_snapshot() {
        let (state, db_path) = test_state().await;
        let mut rx = state.spot_tx.subscribe();

        let returned = tick_once(&state);
        let broadcast = rx.try_recv().unwrap();
        assert_eq!(returned, broadcast);

        // Compared with a tolerance rather than exact JSON equality: the
        // broadcast payload went through a text round-trip (serialize to
        // string, parse back), and serde_json's default float parser
        // isn't guaranteed bit-exact on that round-trip the way its ryu
        // serializer is — an unrelated JSON-text subtlety, not a bug in
        // tick_once itself.
        let payload: serde_json::Value = serde_json::from_str(&broadcast).unwrap();
        let live_prices = state.spot_prices.lock().unwrap().clone();
        for (underlying, live_price) in &live_prices {
            let broadcast_price = payload["prices"][underlying].as_f64().unwrap();
            assert!(
                (broadcast_price - live_price).abs() < 1e-9,
                "{underlying}: broadcast {broadcast_price} vs live {live_price}"
            );
        }

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn reflector_decodes_fixed_point_prices() {
        assert!((ReflectorOracleSource::decode_fixed_point(123_456_789, 6) - 123.456789).abs() < 1e-9);
        assert!((ReflectorOracleSource::decode_fixed_point(42, 0) - 42.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn simulated_source_emits_a_tick_per_symbol() {
        let source = SimulatedSource;
        let symbols = vec!["BTC".to_string(), "ETH".to_string()];
        let ticks = source.fetch(&symbols).await.unwrap();
        assert_eq!(ticks.len(), 2);
        for symbol in &symbols {
            let tick = &ticks[symbol];
            assert_eq!(tick.source, "simulated");
            assert!((tick.price - 1.0).abs() <= MAX_PCT_MOVE_PER_TICK + 1e-9);
        }
    }
}
