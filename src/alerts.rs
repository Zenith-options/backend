use axum::extract::{Path, State};
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

pub async fn delete_alert(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query("DELETE FROM alerts WHERE id = ? AND wallet_address = ?")
        .bind(&id)
        .bind(&wallet_address)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Checks every untriggered alert against the current spot price for its
/// underlying every 10 seconds and flips `triggered` once the condition is
/// met. Alerts stay in the table (and visible via GET) after triggering —
/// they just stop being re-checked — rather than being deleted, so the
/// frontend can show "this alert fired" instead of it silently vanishing.
pub async fn check_alerts_loop(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;

        let prices = state.spot_prices.lock().unwrap().clone();
        for (underlying, spot) in prices {
            let result = sqlx::query(
                "UPDATE alerts
                    SET triggered = 1, triggered_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE underlying = ? AND triggered = 0
                   AND ((condition = 'above' AND target_price <= ?)
                     OR (condition = 'below' AND target_price >= ?))",
            )
            .bind(&underlying)
            .bind(spot)
            .bind(spot)
            .execute(&state.db)
            .await;

            match result {
                Ok(r) if r.rows_affected() > 0 => {
                    tracing::info!(underlying, spot, fired = r.rows_affected(), "alerts triggered");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "alert check failed"),
            }
        }
    }
}
