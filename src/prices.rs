use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rand::Rng;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex};

use crate::AppState;

const MAX_PCT_MOVE_PER_TICK: f64 = 0.003; // +/-0.3%

/// Maximum number of channel subscriptions a single v2 connection may hold.
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 50;

/// How long a v2 connection may sit without sending a subscribe before the
/// server closes it. Prevents idle sockets from pinning resources.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Heartbeat cadence: ping every 20s, and require a pong within 10s.
const PING_INTERVAL: Duration = Duration::from_secs(20);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);

/// A client that stays behind the outbound buffer for longer than this is
/// disconnected with close code 1008 (policy violation).
const MAX_LAG_DURATION: Duration = Duration::from_secs(30);

/// Default per-IP concurrent connection cap.
const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 10;
/// Default global concurrent connection cap.
const DEFAULT_MAX_CONNECTIONS_GLOBAL: usize = 10_000;

/// RFC 6455 close code for a policy violation (slow consumer).
const CLOSE_POLICY_VIOLATION: u16 = 1008;
/// RFC 6455 close code for going away (graceful shutdown).
const CLOSE_GOING_AWAY: u16 = 1001;

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

/// Nudges every spot price by a small random percentage and broadcasts the
/// new snapshot on `state.spot_tx`, returning the JSON payload sent (or
/// not sent, if nothing was listening — that's the common case, not an
/// error). Pulled out of the loop below so a test can assert on the
/// bounds of one tick directly instead of only observing it through a
/// live 2-second timer.
///
/// The new spot/vol pair is published as a single immutable
/// `MarketSnapshot` via read-copy-update, so readers never observe a torn
/// (spot, vol) pair and never take a lock.
pub fn tick_once(state: &AppState) -> String {
    let current = state.market.load();
    let mut prices = current.spot.clone();
    for price in prices.values_mut() {
        let pct_move =
            rand::thread_rng().gen_range(-MAX_PCT_MOVE_PER_TICK..MAX_PCT_MOVE_PER_TICK);
        *price = (*price * (1.0 + pct_move)).max(0.0001);
    }
    let vols = current.vol.clone();

    // Bump the surface version so the surface/term-structure/skew caches
    // (keyed by `(underlying, surface_version)`) are invalidated on every
    // tick, as required by issue #14.
    {
        let mut version = state.surface_version.lock().unwrap();
        *version = version.wrapping_add(1);
    }

    let snapshot = state.publish_snapshot(prices, vols);
    let payload = serde_json::json!({
        "prices": snapshot.spot,
        "vols": snapshot.vol,
    })
    .to_string();
    let _ = state.spot_tx.send(payload.clone());
    payload
}

/// There's no real market feed behind this yet — it exists so the WS
/// endpoint (and the frontend's ticking price displays) has something
/// live to show instead of the static values AppState::new() seeds at
/// startup.
pub async fn price_simulator_loop(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;
        tick_once(&state);
    }
}
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

/// Build the JSON error body used for rejected upgrades, matching the shape
/// used elsewhere in the API.
fn rejection_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    (status, [("content-type", "application/json")], body).into_response()
}

/// Extract the client IP, honouring trusted-proxy forwarding headers when the
/// peer is a trusted proxy. Falls back to the socket peer addr
        }
    }
}

/// Build the JSON error body used for rejected upgrades, matching the shape
/// used elsewhere in the API.
fn rejection_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    (status, [("content-type", "application/json")], body).into_response()
}

/// Extract the client IP, honouring trusted-proxy forwarding headers when the
/// peer is a trusted proxy. Falls back to the socket peer address.
fn client_ip(state: &AppState, addr: &SocketAddr, headers: &axum::http::HeaderMap) -> String {
    crate::rate_limit_key::extract_ip(state, addr, headers)
}

pub async fn ws_spot(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> Response {
    let ip = client_ip(&state, &addr, &headers);
    let guard = match ConnectionGuard::acquire(&state, ip) {
        Some(g) => g,
        None => {
            state.ws_disconnects_total.with_label_values(&["rejected"]).inc();
            return rejection_response(StatusCode::TOO_MANY_REQUESTS, "connection limit reached");
        }
    };
    ws.on_upgrade(move |socket| handle_spot_socket(socket, state, guard))
}

async fn handle_spot_socket(mut socket: WebSocket, state: AppState, _guard: ConnectionGuard) {
    // Send an immediate snapshot so the client has something to render
    // before the first simulator tick (up to 2s away) arrives.
    let snapshot = {
        let prices = state.spot_prices.lock().unwrap().clone();
        let vols = state.vol_surface.lock().unwrap().clone();
        serde_json::json!({ "prices": prices, "vols": vols }).to_string()
    };
    if socket.send(Message::Text(snapshot)).await.is_err() {
        state.ws_disconnects_total.with_label_values(&["send_error"]).inc();
        return;
    }
    state.ws_messages_sent_total.inc();

    let mut rx = state.spot_tx.subscribe();
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // consume the immediate first tick
    let mut awaiting_pong: Option<Instant> = None;
    let mut lag_since: Option<Instant> = None;

    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Ok(payload) => {
                      
    }
    state.ws_messages_sent_total.inc();

    let mut rx = state.spot_tx.subscribe();
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // consume the immediate first tick
    let mut awaiting_pong: Option<Instant> = None;
    let mut lag_since: Option<Instant> = None;

    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Ok(payload) => {
                        lag_since = None;
                        if socket.send(Message::Text(payload)).await.is_err() {
                            state.ws_disconnects_total.with_label_values(&["send_error"]).inc();
                            break;
                        }
                        state.ws_messages_sent_total.inc();
                    }
                    // Client fell behind the broadcast buffer — send a fresh
                    // snapshot plus a resync notice instead of dropping ticks.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        state.ws_lagged_total.inc();
                        let now = Instant::now();
                        let since = *lag_since.get_or_insert(now);
                        if now.duration_since(since) > MAX_LAG_DURATION {
                            let _ = socket
                                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                    code: CLOSE_POLICY_VIOLATION,
                                    reason: "slow consumer".into(),
                                })))
                                .await;
                            state.ws_disconnects_total.with_label_values(&["slow_consumer"]).inc();
                            break;
                        }
                        let snapshot = {
                            let prices = state.spot_prices.lock().unwrap().clone();
                            let vols = state.vol_surface.lock().unwrap().clone();
                            serde_json::json!({ "prices": prices, "vols": vols }).to_string()
                        };
                        if socket.send(Message::Text(snapshot)).await.is_err() {
                            state.ws_disconnects_total.with_label_values(&["send_error"]).inc();
                            break;
                        }
                        state.ws_messages_sent_total.inc();
                        let notice = serde_json::json!({ "type": "resync", "skipped": n }).to_string();
                        if socket.send(Message::Text(notice)).await.is_err() {
                            state.ws_disconnects_total.with_label_values(&["send_error"]).inc();
                            break;
                        }
                        state.ws_messages_sent_total.inc();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = ping_interval.tick() => {
                if let Some(sent) = awaiting_pong {
                    if sent.elapsed() > PONG_TIMEOUT {
                        let _ = socket
                            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: CLOSE_GOING_AWAY,
                                reason: "pong timeout".into(),
                            })))
                            .await;
                        state.ws_disconnects_total.with_label_values(&["pong_timeout"]).inc();
                        break;
                    }
                }
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    state.ws_disconnects_total.with_label_values(&["send_error"]).inc();
                    break;
                }
                awaiting_pong = Some(Instant::now());
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Pong(_))) => {
                        awaiting_pong = None;
                    }
                    Some(Err(_)) => break,
                    _ => {} // ignore anything else the client sends; read-only feed
                }
            }
        }
    }
    state.ws_messages_sent_total.inc();

                }
            }
        }
        self.last_value = Some(value);
        self.tripped
    }

    /// Operator-initiated reset.
    pub fn reset(&mut self) {
        self.tripped = false;
        self.tripped_at = None;
    }
}

/// Fan out to every configured source concurrently under a per-source
/// timeout and aggregate the results for each requested symbol.
pub async fn fetch_aggregated(
    sources: &[Arc<dyn PriceSource>],
    symbols: &[String],
    cfg: &AggregatorConfig,
    timeout: std::time::Duration,
) -> HashMap<String, AggregatedPrice> {
    let mut per_symbol: HashMap<String, Vec<(String, PriceTick)>> = HashMap::new();
    let mut all_sources: Vec<String> = Vec::new();

    let fetches = sources.iter().map(|source| {
        let source = Arc::clone(source);
        let symbols = symbols.to_vec();
        async move {
            let name = source.name().to_string();
            let result = tokio::time::timeout(timeout, source.fetch(&symbols)).await;
            (name, result)
        }
    });

    let results = futures::future::join_all(fetches).await;
    for (name, result) in results {
        all_sources.push(name.clone());
        match result {
            Ok(Ok(ticks)) => {
                for (symbol, tick) in ticks {
                    per_symbol.entry(symbol).or_default().push((name.clone(), tick));
                }
            }
            // A failed source contributes nothing; it is recorded as
            // missing for every symbol it was asked about.
            _ => {}
        }
    }

    let mut out = HashMap::new();
    for symbol in symbols {
        let quotes = per_symbol.remove(symbol).unwrap_or_default();
        let mut agg = aggregate(&quotes, cfg);
        // Record sources that returned nothing for this symbol as missing.
        for source in &all_sources {
            if !quotes.iter().any(|(s, _)| s == source)
                && !agg.rejected.iter().any(|(s, _)| s == source)
            {
                agg.rejected.push((source.clone(), RejectReason::Missing));
            }
        }
        out.insert(symbol.clone(), agg);
    }
    out
}

/// Read-only pricing endpoint. Always responds, but includes a
/// `price_status` field so callers can see whether the price is tradeable.
pub async fn price_handler(
    State(state): State<Arc<AppState>>,
) -> Response {
    let prices = state.aggregated_prices.read().await;
    let body: HashMap<String, serde_json::Value> = prices
        .iter()
        .map(|(symbol, agg)| {
            (
                symbol.clone(),
                serde_json::json!({
                    "price": agg.value,
                    "price_status": agg.status.as_str(),
                    "contributors": agg.contributors,
                    "rejected": agg
                        .rejected
                        .iter()
                        .map(|(s, r)| serde_json::json!({"source": s, "reason": r.to_string()}))
                        .collect::<Vec<_>>(),
                    "as_of": agg.as_of,
                }),
            )
        })
        .collect();
    axum::Json(body).into_response()
}

/// WebSocket upgrade handler for streaming price updates to clients.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.price_tx.subscribe();
    while let Ok(msg) = rx.recv().await {
        if socket.send(Message::Text(msg)).await.is_err() {
            break;
        }
    }
    state.ws_disconnects_total.with_label_values(&["closed"]).inc();
}

/// Multiplexed v2 WebSocket endpoint. One connection can subscribe to any
/// number of public channels (`spot.<U>`, `chain.<U>.<EXPIRY>`,
/// `surface.<U>`) and receives snapshot-then-delta messages with per-channel
/// sequence numbers. The legacy `/api/v1/ws/spot` endpoint above is
/// unchanged and still supported.
pub async fn ws_v2(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> Response {
    let ip = client_ip(&state, &addr, &headers);
    let guard = match ConnectionGuard::acquire(&state, ip) {
        Some(g) => g,
        None => {
            state.ws_disconnects_total.with_label_values(&["rejected"]).inc();
            return rejection_response(StatusCode::TOO_MANY_REQUESTS, "connection limit reached");
        }
    };
    ws.on_upgrade(move |socket| handle_v2_socket(socket, state, guard))
}

/// A single channel's fan-out state: a broadcast sender plus a reference
/// count of live subscribers. The count lets the hub lazily compute chain
/// updates only while at least one client is listening.
struct ChannelState {
    tx: broadcast::Sender<String>,
    subscribers: usize,
}

/// Central hub holding one broadcast channel per subscribed channel name.
/// Shared across all v2 connections via `Arc<Mutex<..>>`.
#[derive(Default)]
struct Hub {
    channels: HashMap<String, ChannelState>,
}

impl Hub {
    /// Subscribe to `channel`, creating the broadcast sender on first use.
    /// Returns the receiver and the current sequence number to stamp on the
    /// snapshot the caller is about to send.
    fn subscribe(&mut self, channel: &str) -> broadcast::Receiver<String> {
        let entry = self.channels.entry(channel.to_string()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(64);
            ChannelState { tx, subscribers: 0 }
        });
        entry.subscribers += 1;
        entry.tx.subscribe()
    }

    /// Drop one subscriber from `channel`, removing the channel entirely
    /// once nobody is left so its broadcast buffer is freed.
    fn unsubscribe(&mut self, channel: &str) {
        if let Some(entry) = self.channels.get_mut(channel) {
            entry.subscribers = entry.subscribers.saturating_sub(1);
            if entry.subscribers == 0 {
                self.channels.remove(channel);
            }
        }
    }

    /// True when at least one client is subscribed to `channel`. Used to
    /// skip computing chain/surface updates nobody is listening for.
    fn has_subscribers(&self, channel: &str) -> bool {
        self.channels.get(channel).map(|c| c.subscribers > 0).unwrap_or(false)
    }

    /// Publish a payload on `channel` if it exists. Returns whether the
    /// channel had any subscribers (i.e. whether the work was worth doing).
    fn publish(&self, channel: &st
        }
    }

    /// True when at least one client is subscribed to `channel`. Used to
    /// skip computing chain/surface updates nobody is listening for.
    fn has_subscribers(&self, channel: &str) -> bool {
        self.channels.get(channel).map(|c| c.subscribers > 0).unwrap_or(false)
    }

    /// Publish a payload on `channel` if it exists. Returns whether the
    /// channel had any subscribers (i.e. whether the work was worth doing).
    fn publish(&self, channel: &str, payload: String) -> bool {
        match self.channels.get(channel) {
            Some(entry) if entry.subscribers > 0 => {
                let _ = entry.tx.send(payload);
                true
            }
            _ => false,
        }
    }

    #[tokio::test]
    async fn tick_once_never_lets_a_price_reach_zero_or_go_negative() {
        let (state, db_path) = test_state().await;
        state.set_spot_for_test("TINY".into(), 0.0001);

        // Enough ticks that a run of unlucky downward moves would drive an
        // unclamped price to zero or below if the floor weren't enforced.
        for _ in 0..1000 {
            tick_once(&state);
        }

        let price = state.market.load().spot["TINY"];
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
        let live_prices = state.market.load().spot.clone();
        for (underlying, live_price) in &live_prices {
            let broadcast_price = payload["prices"][underlying].as_f64().unwrap();
            assert!(
                (broadcast_price - live_price).abs() < 1e-9,
                "{underlying}: broadcast {broadcast_price} vs live {live_price}"
            );
        }
    }

    #[tokio::test]
    async fn tick_once_bounds_price_movement_per_tick() {
        let (state, db_path) = test_state().await;
        let before = state.market.load().spot.clone();

        tick_once(&state);

        let after = state.market.load().spot.clone();
        for (underlying, before_price) in &before {
            let after_price = after[underlying];
            let max_move = before_price * MAX_PCT_MOVE_PER_TICK;
            assert!(
                (after_price - before_price).abs() <= max_move + 1e-9,
                "{underlying} moved from {before_price} to {after_price}, beyond the {MAX_PCT_MOVE_PER_TICK} bound"
            );
        }
    }

        }
    }
}

/// Per-connection subscription bookkeeping: the channel name, its receiver,
/// and the last sequence number sent on it.
struct Subscription {
    channel: String,
    rx: broadcast::Receiver<String>,
    seq: u64,
}

/// Validate a channel name against the supported public channel grammar.
/// Returns `Ok(())` for a well-formed channel, or `Err(reason)` describing
/// why it was rejected. Unknown underlyings are rejected here too so the
/// client gets a structured error instead of a silent no-op.
fn validate_channel(state: &AppState, channel: &str) -> Result<(), String> {
    let parts: Vec<&str> = channel.split('.').collect();
    match parts.as_slice() {
        ["spot", underlying] => {
            if state.spot_prices.lock().unwrap().contains_key(*underlying) {
                Ok(())
            } else {
                Err(format!("unknown underlying: {underlying}"))
            }
        }
        ["surface", underlying] => {
            if state.vol_surface.lock().unwrap().contains_key(*underlying) {
                Ok(())
            } else {
                Err(format!("unknown underlying: {underlying}"))
            }
        }
        ["chain", underlying, expiry] => {
            if !state.spot_prices.lock().unwrap().contains_key(*underlying) {
                return Err(format!("unknown underlying: {underlying}"));
            }
            if expiry.is_empty() {
                return Err("missing expiry".to_string());
            }
            Ok(())
        }
        _ => Err(format!("unknown channel: {channel}")),
    }
}

/// Handle a v2 connection: read subscribe/unsubscribe frames, fan out
/// per-channel messages, and reap the connection on idle timeout.
async fn handle_v2_socket(mut socket: WebSocket, state: AppState, _guard: ConnectionGuard) {
    let hub = state.ws_hub.clone();
    let mut subs: HashMap<String, Subscription> = HashMap::new();
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await;
    let mut awaiting_pong: Option<Instant> = None;
    let idle = tokio::time::sleep(IDLE_TIMEOUT);
    tokio::pin!(idle);

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let msg: serde_json::Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let action = msg.get("action").and_then(|v| v.as_str()).unwrap_or("");
                        let channel = msg.get("channel").and_then(|v| v.as_str()).unwrap_or("");
                        match action {
                            "subscribe" => {
                                if subs.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
                                    continue;
                                }
                                if validate_channel(&state, channel).is_err() {
                                    continue;
                                }
                                let mut hub = hub.lock().await;
                                let rx = hub.subscribe(channel);
                                drop(hub);
                                subs.insert(
                                    channel.to_string(),
                                    Subscription { channel: channel.to_string(), rx, seq: 0 },
                                );
                            }
                            "unsubscribe" => {
                                if subs.remove(channel).is_some() {
                                    hub.lock().await.unsubscribe(channel);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        awaiting_pong = None;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = ping_interval.tick() => {
                if let Some(sent) = awaiting_pong {
                    if sent.elapsed() > PONG_TIMEOUT {
                        let _ = socket
                            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: CLOSE_GOING_AWAY,
                                reason: "pong timeout".into(),
                            })))
                            .await;
                        state.ws_disconnects_total.with_label_values(&["pong_timeout"]).inc();
                        break;
                    }
                }
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
                awaiting_pong = Some(Instant::now());
            }
            _ = &mut idle => {
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: CLOSE_GOING_AWAY,
                        reason: "idle timeout".into(),
                    })))
                    .await;
                state.ws_disconnects_total.with_label_values(&["idle_timeout"]).inc();
                break;
            }
        }
    }

    let mut hub = hub.lock().await;
    for (channel, _) in subs.drain() {
        hub.unsubscribe(&channel);
    }

    /// Hammers reads while the writer publishes new snapshots and asserts
    /// that no reader ever observes a torn (spot, vol) pair: the spot and
    /// vol maps must always come from the same published version.
    #[tokio::test]
    async fn concurrent_reads_never_observe_a_torn_snapshot() {
        let (state, db_path) = test_state().await;

        // Seed a known, version-tagged pair so a torn read is detectable:
        // spot["PAIR"] and vol["PAIR"] must always carry the sam
    }

    /// Hammers reads while the writer publishes new snapshots and asserts
    /// that no reader ever observes a torn (spot, vol) pair: the spot and
    /// vol maps must always come from the same published version.
    #[tokio::test]
    async fn concurrent_reads_never_observe_a_torn_snapshot() {
        let (state, db_path) = test_state().await;

        // Seed a known, version-tagged pair so a torn read is detectable:
        // spot["PAIR"] and vol["PAIR"] must always carry the same version.
        let mut spot = state.market.load().spot.clone();
        let mut vol = state.market.load().vol.clone();
        spot.insert("PAIR".into(), 1.0);
        vol.insert("PAIR".into(), 1.0);
        state.publish_snapshot(spot, vol);

        let writer = {
            let state = state.clone();
            tokio::spawn(async move {
                for version in 2..500u64 {
                    let mut spot = state.market.load().spot.clone();
                    let mut vol = state.market.load().vol.clone();
                    spot.insert("PAIR".into(), version as f64);
                    vol.insert("PAIR".into(), version as f64);
                    state.publish_snapshot(spot, vol);
                    tokio::task::yield_now().await;
                }
            })
        };

        let mut readers = Vec::new();
        for _ in 0..8 {
            let state = state.clone();
            readers.push(tokio::spawn(async move {
                for _ in 0..2000 {
                    let market = state.market.load();
                    let spot = market.spot["PAIR"];
                    let vol = market.vol["PAIR"];
                    assert_eq!(
                        spot, vol,
                        "torn read: spot {spot} paired with vol {vol}"
                    );
                    tokio::task::yield_now().await;
                }
            }));
        }

        writer.await.unwrap();
        for reader in readers {
            reader.await.unwrap();
        }

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn tick_once_bumps_the_surface_version() {
        let (state, db_path) = test_state().await;
        let before = *state.surface_version.lock().unwrap();

        tick_once(&state);

        let after = *state.surface_version.lock().unwrap();
        assert_eq!(after, before.wrapping_add(1));

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }
}
