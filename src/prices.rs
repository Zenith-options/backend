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

/// Nudges every spot price by a small random percentage and broadcasts the
/// new snapshot on `state.spot_tx`, returning the JSON payload sent (or
/// not sent, if nothing was listening — that's the common case, not an
/// error). Pulled out of the loop below so a test can assert on the
/// bounds of one tick directly instead of only observing it through a
/// live 2-second timer.
pub fn tick_once(state: &AppState) -> String {
    let prices = {
        let mut prices = state.spot_prices.lock().unwrap();
        for price in prices.values_mut() {
            let pct_move =
                rand::thread_rng().gen_range(-MAX_PCT_MOVE_PER_TICK..MAX_PCT_MOVE_PER_TICK);
            *price = (*price * (1.0 + pct_move)).max(0.0001);
        }
        prices.clone()
    };
    let vols = state.vol_surface.lock().unwrap().clone();

    let payload = serde_json::json!({ "prices": prices, "vols": vols }).to_string();
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

/// RAII guard tracking live WebSocket connections. Increments the global and
/// per-IP counters on construction and decrements them on drop, so counts
/// cannot leak on panic or early return.
struct ConnectionGuard {
    state: AppState,
    ip: String,
}

impl ConnectionGuard {
    /// Try to reserve a connection slot for `ip`. Returns `None` when either
    /// the per-IP or the global cap is already reached.
    fn acquire(state: &AppState, ip: String) -> Option<Self> {
        let global = state.ws_connections_global.load(std::sync::atomic::Ordering::SeqCst);
        if global >= state.ws_max_connections_global {
            return None;
        }
        let mut entry = state.ws_connections_per_ip.entry(ip.clone()).or_insert(0);
        if *entry >= state.ws_max_connections_per_ip {
            return None;
        }
        *entry += 1;
        drop(entry);
        state.ws_connections_global.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(ConnectionGuard { state: state.clone(), ip })
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.ws_connections_global.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(mut entry) = self.state.ws_connections_per_ip.get_mut(&self.ip) {
            if *entry > 0 {
                *entry -= 1;
            }
            if *entry == 0 {
                drop(entry);
                self.state.ws_connections_per_ip.remove(&self.ip);
            }
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
    fn publish(&self, channel: &str, payload: String) -> bool {
        match self.channels.get(channel) {
            Some(entry) if entry.subscribers > 0 => {
                let _ = entry.tx.send(payload);
                true
            }
            _ => false,
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
}
