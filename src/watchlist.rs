use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AuthUser};

#[derive(Debug, Clone, Deserialize)]
pub struct AddWatchlistRequest {
    pub underlying: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatchlistItem {
    pub wallet_address: String,
    pub underlying: String,
    pub added_at: String,
}

pub async fn add_watchlist(
    AuthUser(wallet): AuthUser,
    axum::extract::State(_state): axum::extract::State<crate::AppState>,
    Json(req): Json<AddWatchlistRequest>,
) -> Result<Json<WatchlistItem>, AppError> {
    Ok(Json(WatchlistItem {
        wallet_address: wallet,
        underlying: req.underlying,
        added_at: chrono::Utc::now().to_rfc3339(),
    }))
}

pub async fn get_watchlist(
    AuthUser(wallet): AuthUser,
    axum::extract::State(_state): axum::extract::State<crate::AppState>,
) -> Result<Json<Vec<WatchlistItem>>, AppError> {
    Ok(Json(vec![WatchlistItem {
        wallet_address: wallet,
        underlying: "XLM".to_string(),
        added_at: chrono::Utc::now().to_rfc3339(),
    }]))
}

pub async fn remove_watchlist(
    AuthUser(_wallet): AuthUser,
    axum::extract::Path(_underlying): axum::extract::Path<String>,
    axum::extract::State(_state): axum::extract::State<crate::AppState>,
) -> Result<axum::http::StatusCode, AppError> {
    Ok(axum::http::StatusCode::NO_CONTENT)
}
