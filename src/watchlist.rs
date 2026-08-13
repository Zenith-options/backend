use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::models::WatchlistItem;
use crate::AppState;

pub async fn get_watchlist(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Vec<WatchlistItem>>, StatusCode> {
    let items: Vec<WatchlistItem> = sqlx::query_as(
        "SELECT * FROM watchlist WHERE wallet_address = ? ORDER BY added_at DESC",
    )
    .bind(&wallet_address)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(items))
}

#[derive(Deserialize)]
pub struct AddWatchlistRequest {
    pub underlying: String,
}

pub async fn add_watchlist(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Json(req): Json<AddWatchlistRequest>,
) -> Result<StatusCode, StatusCode> {
    if !state.spot_prices.lock().unwrap().contains_key(&req.underlying) {
        return Err(StatusCode::NOT_FOUND);
    }

    sqlx::query(
        "INSERT INTO watchlist (wallet_address, underlying) VALUES (?, ?)
         ON CONFLICT(wallet_address, underlying) DO NOTHING",
    )
    .bind(&wallet_address)
    .bind(&req.underlying)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::CREATED)
}
