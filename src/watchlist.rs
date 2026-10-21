use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{db_error, AppError, ValidatedJson};
use crate::models::WatchlistItem;
use crate::AppState;

pub const CHAIN_EVENT_TYPES: [&str; 5] = [
    "settlement_completed",
    "margin_call_threshold",
    "liquidation_warning",
    "delivery_status",
    "vault_share_price_move",
];

#[derive(Debug, Serialize)]
pub struct WatchlistEntry {
    pub wallet_address: String,
    pub underlying: String,
    pub added_at: String,
    pub event_types: Vec<String>,
}

pub async fn get_watchlist(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Vec<WatchlistEntry>>, AppError> {
    let items: Vec<WatchlistItem> = sqlx::query_as(
        "SELECT wallet_address, underlying, added_at FROM watchlist
         WHERE wallet_address = ? ORDER BY added_at DESC",
    )
    .bind(&wallet_address)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("load watchlist", e))?;

    let subscriptions: Vec<(String, String)> = sqlx::query_as(
        "SELECT underlying, event_type FROM watchlist_event_subscriptions
         WHERE wallet_address = ? ORDER BY underlying, event_type",
    )
    .bind(&wallet_address)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("load watchlist event subscriptions", e))?;

    let mut event_types = std::collections::HashMap::<String, Vec<String>>::new();
    for (underlying, event_type) in subscriptions {
        event_types.entry(underlying).or_default().push(event_type);
    }

    Ok(Json(
        items
            .into_iter()
            .map(|item| WatchlistEntry {
                wallet_address: item.wallet_address,
                event_types: event_types.remove(&item.underlying).unwrap_or_default(),
                underlying: item.underlying,
                added_at: item.added_at,
            })
            .collect(),
    ))
}

#[derive(Deserialize, validator::Validate)]
pub struct AddWatchlistRequest {
    #[validate(length(min = 1, max = 32))]
    pub underlying: String,
    #[serde(default)]
    pub event_types: Vec<String>,
}

pub async fn add_watchlist(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    ValidatedJson(req): ValidatedJson<AddWatchlistRequest>,
) -> Result<StatusCode, AppError> {
    if req.event_types.len() > CHAIN_EVENT_TYPES.len()
        || req
            .event_types
            .iter()
            .any(|event_type| !CHAIN_EVENT_TYPES.contains(&event_type.as_str()))
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "event_types must contain up to {} supported chain event types",
                CHAIN_EVENT_TYPES.len()
            ),
        ));
    }

    if !state
        .spot_prices
        .lock()
        .unwrap()
        .contains_key(&req.underlying)
    {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            format!("unknown underlying \"{}\"", req.underlying),
        ));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| db_error("begin watchlist update", e))?;
    sqlx::query(
        "INSERT INTO watchlist (wallet_address, underlying) VALUES (?, ?) ON CONFLICT(wallet_address, underlying) DO NOTHING",
    )
    .bind(&wallet_address)
    .bind(&req.underlying)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_error("add watchlist item", e))?;

    for event_type in req.event_types {
        sqlx::query(
            "INSERT INTO watchlist_event_subscriptions
                (wallet_address, underlying, event_type)
             VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(&wallet_address)
        .bind(&req.underlying)
        .bind(event_type)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_error("subscribe to chain event", e))?;
    }
    tx.commit()
        .await
        .map_err(|e| db_error("commit watchlist update", e))?;

    Ok(StatusCode::CREATED)
}

pub async fn remove_watchlist(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Path(underlying): Path<String>,
) -> Result<StatusCode, AppError> {
    let result = sqlx::query(
        "DELETE FROM watchlist WHERE wallet_address = ? AND underlying = ?",
    )
    .bind(&wallet_address)
    .bind(&underlying)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("remove watchlist item", e))?;

    if result.rows_affected() == 0 {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            format!("\"{underlying}\" is not on this wallet's watchlist"),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}
