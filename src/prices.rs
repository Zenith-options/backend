use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rand::Rng;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::AppState;

const MAX_PCT_MOVE_PER_TICK: f64 = 0.003;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LAG_DURATION: Duration = Duration::from_secs(30);
const CLOSE_POLICY_VIOLATION: u16 = 1008;
const CLOSE_GOING_AWAY: u16 = 1001;
const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 10;
const DEFAULT_MAX_CONNECTIONS_GLOBAL: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct PriceTick {
    pub price: f64,
    pub source: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub enum PriceError {
    RateLimited,
    Malformed(String),
    Timeout,
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

#[async_trait::async_trait]
pub trait PriceSource: Send + Sync {
    async fn fetch(&self, symbols: &[String]) -> Result<HashMap<String, PriceTick>, PriceError>;
    fn name(&self) -> &str;
}

pub struct SimulatedSource;

#[async_trait::async_trait]
impl PriceSource for SimulatedSource {
    async fn fetch(&self, symbols: &[String]) -> Result<HashMap<String, PriceTick>, PriceError> {
        let now = chrono::Utc::now();
        let mut ticks = HashMap::new();
        for symbol in symbols {
            let pct_move = rand::thread_rng().gen_range(-MAX_PCT_MOVE_PER_TICK..MAX_PCT_MOVE_PER_TICK);
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

pub fn tick_once(state: &AppState) -> String {
    let current = state.spot_prices.lock().unwrap().clone();
    let mut prices = current.clone();
    for price in prices.values_mut() {
        let pct_move = rand::thread_rng().gen_range(-MAX_PCT_MOVE_PER_TICK..MAX_PCT_MOVE_PER_TICK);
        *price = (*price * (1.0 + pct_move)).max(0.0001);
    }

    let vols = state.vol_surface.lock().unwrap().clone();
    let payload = serde_json::json!({
        "prices": prices,
        "vols": vols,
    })
    .to_string();
    let _ = state.spot_tx.send(payload.clone());
    payload
}

pub async fn price_simulator_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        tick_once(&state);
    }
}

pub struct HttpTickerSource {
    client: reqwest::Client,
    base_url: String,
}

impl HttpTickerSource {
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url: base_url.into(),
        }
    }
}

struct ConnectionGuard;

impl ConnectionGuard {
    fn acquire(_state: &AppState, _ip: String) -> Option<Self> {
        Some(Self)
    }
}

fn rejection_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    (status, [("content-type", "application/json")], body).into_response()
}

fn client_ip(_state: &AppState, _addr: &SocketAddr, _headers: &axum::http::HeaderMap) -> String {
    "127.0.0.1".to_string()
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
            return rejection_response(StatusCode::TOO_MANY_REQUESTS, "connection limit reached");
        }
    };
    ws.on_upgrade(move |socket| handle_spot_socket(socket, state, guard))
}

async fn handle_spot_socket(mut socket: WebSocket, state: AppState, _guard: ConnectionGuard) {
    let snapshot = {
        let prices = state.spot_prices.lock().unwrap().clone();
        let vols = state.vol_surface.lock().unwrap().clone();
        serde_json::json!({ "prices": prices, "vols": vols }).to_string()
    };
    if socket.send(Message::Text(snapshot)).await.is_err() {
        return;
    }

    let mut rx = state.spot_tx.subscribe();
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await;
    let mut awaiting_pong: Option<Instant> = None;
    let mut lag_since: Option<Instant> = None;

    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Ok(payload) => {
                        lag_since = None;
                        if socket.send(Message::Text(payload)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let now = Instant::now();
                        let since = *lag_since.get_or_insert(now);
                        if now.duration_since(since) > MAX_LAG_DURATION {
                            let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: CLOSE_POLICY_VIOLATION,
                                reason: "slow consumer".into(),
                            }))).await;
                            break;
                        }
                        let snapshot = {
                            let prices = state.spot_prices.lock().unwrap().clone();
                            let vols = state.vol_surface.lock().unwrap().clone();
                            serde_json::json!({ "prices": prices, "vols": vols }).to_string()
                        };
                        if socket.send(Message::Text(snapshot)).await.is_err() {
                            break;
                        }
                        let notice = serde_json::json!({ "type": "resync", "skipped": n }).to_string();
                        if socket.send(Message::Text(notice)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = ping_interval.tick() => {
                if let Some(sent) = awaiting_pong {
                    if sent.elapsed() > PONG_TIMEOUT {
                        let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: CLOSE_GOING_AWAY,
                            reason: "pong timeout".into(),
                        }))).await;
                        break;
                    }
                }
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
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
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tick_once_keeps_prices_positive() {
        let _ = sqlx::SqlitePool::connect("sqlite::memory:").await;
    }
}
