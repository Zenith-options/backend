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

use std::time::Duration;
use crate::price_sources::PriceSource;

const SYMBOLS: &[&str] = &["XLM", "BTC", "ETH", "SOL"];

pub async fn price_ingestion_loop(state: crate::AppState) {
    let symbols = SYMBOLS
        .iter()
        .map(|symbol| (*symbol).to_string())
        .collect::<Vec<_>>();
    loop {
        match state.price_source.fetch(&symbols).await {
            Ok(quotes) => {
                let mut prices = state.spot_prices.lock().unwrap();
                let mut ticks = state.price_ticks.lock().unwrap();
                for (symbol, tick) in quotes {
                    if !tick.price.is_finite() || tick.price <= 0.0 {
                        tracing::warn!(symbol, "ignoring invalid observed market price");
                        continue;
                    }
                    prices.insert(symbol.clone(), tick.price);
                    ticks.insert(symbol, tick);
                }
                let payload = crate::SpotResponse {
                    prices: prices.clone(),
                    vols: state.vol_surface.lock().unwrap().clone(),
                    sources: ticks
                        .iter()
                        .map(|(symbol, tick)| (symbol.clone(), tick.source.clone()))
                        .collect(),
                };
                match serde_json::to_string(&payload) {
                    Ok(encoded) => {
                        let _ = state.spot_tx.send(encoded);
                    }
                    Err(error) => {
                        tracing::error!(%error, "could not serialize market-price update");
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "market price ingestion failed; retaining last verified prices");
            }
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

