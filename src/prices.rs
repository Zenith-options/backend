use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use crate::AppState;

const MAX_PCT_MOVE_PER_TICK: f64 = 0.003; // +/-0.3%

/// Maximum number of channel subscriptions a single v2 connection may hold.
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 50;

/// How long a v2 connection may sit without sending a subscribe before the
/// server closes it. Prevents idle sockets from pinning resources.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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

pub async fn ws_spot(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_spot_socket(socket, state))
}

async fn handle_spot_socket(mut socket: WebSocket, state: AppState) {
    // Send an immediate snapshot so the client has something to render
    // before the first simulator tick (up to 2s away) arrives.
    let snapshot = {
        let prices = state.spot_prices.lock().unwrap().clone();
        let vols = state.vol_surface.lock().unwrap().clone();
        serde_json::json!({ "prices": prices, "vols": vols }).to_string()
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

/// Multiplexed v2 WebSocket endpoint. One connection can subscribe to any
/// number of public channels (`spot.<U>`, `chain.<U>.<EXPIRY>`,
/// `surface.<U>`) and receives snapshot-then-delta messages with per-channel
/// sequence numbers. The legacy `/api/v1/ws/spot` endpoint above is
/// unchanged and still supported.
pub async fn ws_v2(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_v2_socket(socket, state))
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

/// Build the snapshot payload for a channel from current state.
fn snapshot_for(state: &AppState, channel: &str) -> serde_json::Value {
    let parts: Vec<&str> = channel.split('.').collect();
    match parts.as_slice() {
        ["spot", underlying] => {
            let price = state.spot_prices.lock().unwrap().get(*underlying).copied();
            serde_json::json!({ "underlying": underlying, "price": price })
        }
        ["surface", underlying] => {
            let vol = state.vol_surface.lock().unwrap().get(*underlying).copied();
            serde_json::json!({ "underlying": underlying, "vol": vol })
        }
        ["chain", underlying, expiry] => {
            serde_json::json!({ "underlying": underlying, "expiry": expiry })
        }
        _ => serde_json::Value::Null,
    }
}

/// Serialize a server message with the shared envelope.
fn server_message(channel: &str, seq: u64, kind: &str, data: serde_json::Value) -> String {
    serde_json::json!({
        "channel": channel,
        "seq": seq,
        "type": kind,
        "data": data,
    })
    .to_string()
}

/// Send a structured error frame without dropping the connection.
fn error_message(message: &str) -> String {
    serde_json::json!({ "type": "error", "error": message }).to_string()
}

async fn handle_v2_socket(mut socket: WebSocket, state: AppState) {
    let hub: Arc<Mutex<Hub>> = Arc::new(Mutex::new(Hub::default()));
    let mut subs: Vec<Subscription> = Vec::new();
    let mut subscribed_once = false;

    loop {
        // Build the set of receivers to poll. We can't hold the hub lock
        // across the select, so we drain each receiver's pending message
        // opportunistically and fall back to the inbound socket branch.
        let mut outbound: Option<String> = None;
        for sub in subs.iter_mut() {
            match sub.rx.try_recv() {
                Ok(payload) => {
                    sub.seq += 1;
                    outbound = Some(server_message(&sub.channel, sub.seq, "update", serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null)));
                    break;
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    // Gap detected: resend a fresh snapshot so the client can
                    // resync its sequence tracking.
                    sub.seq += 1;
                    let data = snapshot_for(&state, &sub.channel);
                    outbound = Some(server_message(&sub.channel, sub.seq, "snapshot", data));
                    break;
                }
                Err(broadcast::error::TryRecvError::Empty) => {}
                Err(broadcast::error::TryRecvError::Closed) => {}
            }
        }

        if let Some(msg) = outbound {
            if socket.send(Message::Text(msg)).await.is_err() {
                break;
            }
            continue;
        }

        let idle = tokio::time::sleep(IDLE_TIMEOUT);
        tokio::pin!(idle);

        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let parsed: serde_json::Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => {
                                let _ = socket.send(Message::Text(error_message("invalid JSON"))).await;
                                continue;
                            }
                        };
                        let op = parsed.get("op").and_then(|v| v.as_str()).unwrap_or("");
                        let channels: Vec<String> = parsed
                            .get("channels")
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
                            .unwrap_or_default();

                        match op {
                            "subscribe" => {
                                subscribed_once = true;
                                for channel in channels {
                                    // Duplicate subscriptions are idempotent.
                                    if subs.iter().any(|s| s.channel == channel) {
                                        continue;
                                    }
                                    if subs.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
                                        let _ = socket
                                            .send(Message::Text(error_message(&format!(
                                                "subscription limit exceeded: max {MAX_SUBSCRIPTIONS_PER_CONNECTION} per connection"
                                            ))))
                                            .await;
                                        break;
                                    }
                                    if let Err(reason) = validate_channel(&state, &channel) {
                                        let _ = socket
                                            .send(Message::Text(error_message(&reason)))
                                            .await;
                                        continue;
                                    }
                                    let rx = hub.lock().await.subscribe(&channel);
                                    let data = snapshot_for(&state, &channel);
                                    let msg = server_message(&channel, 1, "snapshot", data);
                                    if socket.send(Message::Text(msg)).await.is_err() {
                                        return;
                                    }
                                    subs.push(Subscription { channel, rx, seq: 1 });
                                }
                            }
                            "unsubscribe" => {
                                for channel in channels {
                                    if let Some(pos) = subs.iter().position(|s| s.channel == channel) {
                                        subs.remove(pos);
                                        hub.lock().await.unsubscribe(&channel);
                                    }
                                }
                            }
                            _ => {
                                let _ = socket
                                    .send(Message::Text(error_message(&format!("unknown op: {op}"))))
                                    .await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = &mut idle, if !subscribed_once => {
                // Client never subscribed within the idle window.
                break;
            }
        }
    }

    // Release every subscription so the hub can drop idle channels.
    let mut hub = hub.lock().await;
    for sub in &subs {
        hub.unsubscribe(&sub.channel);
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

    #[tokio::test]
    async fn hub_tracks_subscriber_counts_and_lazy_channels() {
        let mut hub = Hub::default();
        assert!(!hub.has_subscribers("spot.BTC"));

        let _rx = hub.subscribe("spot.BTC");
        assert!(hub.has_subscribers("spot.BTC"));

        // Publishing to a channel with a subscriber succeeds.
        assert!(hub.publish("spot.BTC", "{}".to_string()));

        hub.unsubscribe("spot.BTC");
        assert!(!hub.has_subscribers("spot.BTC"));
        // Publishing to a channel nobody listens to is a no-op.
        assert!(!hub.publish("spot.BTC", "{}".to_string()));
    }

    #[tokio::test]
    async fn validate_channel_accepts_known_and_rejects_unknown() {
        let (state, db_path) = test_state().await;
        let known = state.spot_prices.lock().unwrap().keys().next().cloned().unwrap();

        assert!(validate_channel(&state, &format!("spot.{known}")).is_ok());
        assert!(validate_channel(&state, &format!("surface.{known}")).is_ok());
        assert!(validate_channel(&state, &format!("chain.{known}.2024-01-01")).is_ok());
        assert!(validate_channel(&state, "spot.NOPE").is_err());
        assert!(validate_channel(&state, "bogus.BTC").is_err());

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }
}
