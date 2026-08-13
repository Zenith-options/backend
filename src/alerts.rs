use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::models::Alert;
use crate::AppState;

pub async fn get_alerts(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Vec<Alert>>, StatusCode> {
    let alerts: Vec<Alert> = sqlx::query_as(
        "SELECT * FROM alerts WHERE wallet_address = ? ORDER BY created_at DESC",
    )
    .bind(&wallet_address)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(alerts))
}

#[derive(Deserialize)]
pub struct CreateAlertRequest {
    pub underlying: String,
    pub condition: String, // "above" | "below"
    pub target_price: f64,
}

pub async fn create_alert(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Json(req): Json<CreateAlertRequest>,
) -> Result<Json<Alert>, StatusCode> {
    if req.condition != "above" && req.condition != "below" {
        return Err(StatusCode::BAD_REQUEST);
    }
    if req.target_price <= 0.0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !state.spot_prices.lock().unwrap().contains_key(&req.underlying) {
        return Err(StatusCode::NOT_FOUND);
    }

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO alerts (id, wallet_address, underlying, condition, target_price)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&wallet_address)
    .bind(&req.underlying)
    .bind(&req.condition)
    .bind(req.target_price)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let alert: Alert = sqlx::query_as("SELECT * FROM alerts WHERE id = ?")
        .bind(&id)
        .fetch_one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(alert))
}
