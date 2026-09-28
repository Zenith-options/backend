use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::AppState;

const MAX_PCT_MOVE_PER_TICK: f64 = 0.003; // +/-0.3%

#[derive(Deserialize, Serialize)]
struct MarketStatePayload {
    prices: std::collections::HashMap<String, f64>,
    vols: std::collections::HashMap<String, f64>,
}

pub async fn initialize_market_state(state: &AppState) -> Result<(), sqlx::Error> {
    let initial_payload = market_payload(state);
    sqlx::query("INSERT OR IGNORE INTO market_state (id, payload) VALUES (1, ?)")
        .bind(initial_payload)
        .execute(&state.db)
        .await?;
    refresh_market_state(state).await?;
    Ok(())
}

fn market_payload(state: &AppState) -> String {
    let payload = MarketStatePayload {
        prices: state
            .spot_prices
            .lock()
            .expect("price cache lock poisoned")
            .clone(),
        vols: state
            .vol_surface
            .lock()
            .expect("vol cache lock poisoned")
            .clone(),
    };
    serde_json::to_string(&payload).expect("market state serializes")
}

fn apply_market_payload(state: &AppState, payload: &str) -> Result<bool, serde_json::Error> {
    let market: MarketStatePayload = serde_json::from_str(payload)?;
    let changed = {
        let mut prices = state.spot_prices.lock().expect("price cache lock poisoned");
        let mut vols = state.vol_surface.lock().expect("vol cache lock poisoned");
        let changed = *prices != market.prices || *vols != market.vols;
        *prices = market.prices;
        *vols = market.vols;
        changed
    };
    if changed {
        let _ = state.spot_tx.send(payload.to_string());
    }
    Ok(changed)
}

pub async fn refresh_market_state(state: &AppState) -> Result<(), sqlx::Error> {
    let payload: Option<(String,)> =
        sqlx::query_as("SELECT payload FROM market_state WHERE id = 1")
            .fetch_optional(&state.db)
            .await?;
    if let Some((payload,)) = payload {
        apply_market_payload(state, &payload)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    }
    Ok(())
}

pub async fn refresh_market_state_loop(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        if let Err(error) = refresh_market_state(&state).await {
            tracing::error!(error = %error, "refresh shared market state failed");
        }
    }
}

/// Nudges every spot price by a small random percentage and broadcasts the
/// new snapshot on `state.spot_tx`, returning the JSON payload sent (or
/// not sent, if nothing was listening — that's the common case, not an
/// error). Pulled out of the loop below so a test can assert on the
/// bounds of one tick directly and reused by the worker job.
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

pub async fn tick_and_persist(state: &AppState) -> Result<String, sqlx::Error> {
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE market_state SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = 1",
    )
    .execute(&mut *tx)
    .await?;
    let (current_payload,): (String,) =
        sqlx::query_as("SELECT payload FROM market_state WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    apply_market_payload(state, &current_payload)
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    let payload = tick_once(state);
    sqlx::query(
        "UPDATE market_state SET payload = ?,
           updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = 1",
    )
    .bind(&payload)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(payload)
}

pub async fn ws_spot(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_spot_socket(socket, state))
}

async fn handle_spot_socket(mut socket: WebSocket, state: AppState) {
    // Send an immediate snapshot so the client has something to render
    // before the next worker tick arrives.
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
    async fn worker_market_updates_are_visible_to_a_separate_api_state() {
        let (worker_state, db_path) = test_state().await;
        initialize_market_state(&worker_state).await.unwrap();
        let second_worker = AppState::new(worker_state.db.clone());
        initialize_market_state(&second_worker).await.unwrap();
        worker_state
            .spot_prices
            .lock()
            .unwrap()
            .insert("BTC".into(), 71_000.0);
        tick_and_persist(&worker_state).await.unwrap();
        tick_and_persist(&second_worker).await.unwrap();

        let api_state = AppState::new(worker_state.db.clone());
        initialize_market_state(&api_state).await.unwrap();
        assert_eq!(
            api_state.spot_prices.lock().unwrap()["BTC"],
            second_worker.spot_prices.lock().unwrap()["BTC"]
        );

        worker_state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }
}
