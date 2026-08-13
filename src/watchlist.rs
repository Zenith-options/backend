use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

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
