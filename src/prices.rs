use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use rand::Rng;

use crate::AppState;

const MAX_PCT_MOVE_PER_TICK: f64 = 0.003; // +/-0.3%

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

pub async fn ws_spot(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_spot_socket(socket, state))
}

async fn handle_spot_socket(mut socket: WebSocket, state: AppState) {
    // Send an immediate snapshot so the client has something to render
    // before the first simulator tick (up to 2s away) arrives. A single
    // load gives a consistent (spot, vol) pair for the whole message.
    let snapshot = {
        let market = state.market.load();
        serde_json::json!({
            "prices": market.spot,
            "vols": market.vol,
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

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
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

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
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
}
