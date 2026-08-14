use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::error::{db_error, AppError, AppJson};
use crate::models::Position;
use crate::positions::{open_position_in_tx, OpenPositionRequest};
use crate::AppState;

#[derive(Deserialize)]
pub struct ExecuteStrategyRequest {
    pub legs: Vec<OpenPositionRequest>,
}

/// Opens every leg of a multi-leg strategy (straddle, spread, iron
/// condor, ...) as one atomic transaction under a shared strategy_id —
/// either all legs open or none do, so a mid-strategy insufficient-funds
/// rejection can't leave a naked partial position behind.
pub async fn execute_strategy(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    AppJson(req): AppJson<ExecuteStrategyRequest>,
) -> Result<Json<Vec<Position>>, AppError> {
    if req.legs.len() < 2 {
        // A single "strategy" leg is just a plain open — use
        // /api/v1/positions/open for that instead.
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "a strategy needs at least 2 legs; use /api/v1/positions/open for a single leg",
        ));
    }

    let strategy_id = uuid::Uuid::new_v4().to_string();
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| db_error("begin strategy transaction", e))?;

    let mut opened = Vec::with_capacity(req.legs.len());
    for leg in &req.legs {
        let position =
            open_position_in_tx(&mut tx, &state, &wallet_address, leg, Some(&strategy_id)).await?;
        opened.push(position);
    }

    tx.commit()
        .await
        .map_err(|e| db_error("commit strategy transaction", e))?;
    Ok(Json(opened))
}
