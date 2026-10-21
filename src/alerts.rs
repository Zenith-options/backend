use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AuthUser};

#[derive(Debug, Clone, Deserialize)]
pub struct CreateAlertRequest {
    pub underlying: String,
    pub condition: String,
    pub target_price: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    pub id: String,
    pub wallet_address: String,
    pub underlying: String,
    pub condition: String,
    pub target_price: f64,
}

pub async fn check_alerts_loop(_state: crate::AppState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

pub async fn create_alert(
    AuthUser(wallet): AuthUser,
    axum::extract::State(_state): axum::extract::State<crate::AppState>,
    Json(req): Json<CreateAlertRequest>,
) -> Result<Json<Alert>, AppError> {
    Ok(Json(Alert {
        id: uuid::Uuid::new_v4().to_string(),
        wallet_address: wallet,
        underlying: req.underlying,
        condition: req.condition,
        target_price: req.target_price,
    }))
}

pub async fn get_alerts(
    AuthUser(wallet): AuthUser,
    axum::extract::State(_state): axum::extract::State<crate::AppState>,
) -> Result<Json<Vec<Alert>>, AppError> {
    Ok(Json(vec![Alert {
        id: uuid::Uuid::new_v4().to_string(),
        wallet_address: wallet,
        underlying: "XLM".to_string(),
        condition: "above".to_string(),
        target_price: 0.2,
    }]))
}

pub async fn delete_alert(
    AuthUser(_wallet): AuthUser,
    axum::extract::Path(_id): axum::extract::Path<String>,
    axum::extract::State(_state): axum::extract::State<crate::AppState>,
) -> Result<axum::http::StatusCode, AppError> {
    Ok(axum::http::StatusCode::NO_CONTENT)
}
