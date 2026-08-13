use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use rand::Rng;

use crate::AppState;

/// Nudges every spot price by a small random percentage every 2 seconds
/// and broadcasts the new snapshot on `state.spot_tx`. There's no real
/// market feed behind this yet — it exists so the WS endpoint (and the
/// frontend's ticking price displays) has something live to show instead
/// of the static values AppState::new() seeds at startup.
pub async fn price_simulator_loop(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;

        let prices = {
            let mut prices = state.spot_prices.lock().unwrap();
            for price in prices.values_mut() {
                let pct_move = rand::thread_rng().gen_range(-0.003..0.003); // +/-0.3% per tick
                *price = (*price * (1.0 + pct_move)).max(0.0001);
            }
            prices.clone()
        };
        let vols = state.vol_surface.lock().unwrap().clone();

        let payload = serde_json::json!({ "prices": prices, "vols": vols }).to_string();
        // No receivers connected is the common case, not an error.
        let _ = state.spot_tx.send(payload);
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
