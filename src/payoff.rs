use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppJson};

#[derive(Debug, Clone, Deserialize)]
pub struct PayoffRequest {
    pub legs: Vec<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PayoffResponse {
    pub payoff: f64,
}

pub async fn post_payoff(Json(req): Json<PayoffRequest>) -> Result<Json<PayoffResponse>, AppError> {
    let payoff = req.legs.iter().sum::<f64>();
    Ok(Json(PayoffResponse { payoff }))
}
