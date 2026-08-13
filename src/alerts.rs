use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

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
