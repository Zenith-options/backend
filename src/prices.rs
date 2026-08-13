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
