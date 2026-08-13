use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::models::{Account, Position};
use crate::AppState;

pub async fn get_account(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Account>, StatusCode> {
    // Verify/login already creates this row, but stay defensive in case a
    // session outlives some future account-deletion path.
    sqlx::query(
        "INSERT INTO accounts (wallet_address) VALUES (?) ON CONFLICT(wallet_address) DO NOTHING",
    )
    .bind(&wallet_address)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(&wallet_address)
        .fetch_one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(account))
}

#[derive(Deserialize)]
pub struct ListPositionsQuery {
    /// "open" | "closed" | "rolled" — omit to return every status.
    pub status: Option<String>,
}

pub async fn list_positions(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Query(q): Query<ListPositionsQuery>,
) -> Result<Json<Vec<Position>>, StatusCode> {
    let positions: Vec<Position> = match q.status {
        Some(status) => {
            sqlx::query_as(
                "SELECT * FROM positions WHERE wallet_address = ? AND status = ? ORDER BY opened_at DESC",
            )
            .bind(&wallet_address)
            .bind(&status)
            .fetch_all(&state.db)
            .await
        }
        None => {
            sqlx::query_as("SELECT * FROM positions WHERE wallet_address = ? ORDER BY opened_at DESC")
                .bind(&wallet_address)
                .fetch_all(&state.db)
                .await
        }
    }
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(positions))
}
