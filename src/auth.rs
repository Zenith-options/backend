use axum::Json;
use serde::{Deserialize, Serialize};

pub use crate::error::AuthUser;
use crate::error::{AppError, AppJson};

#[derive(Debug, Deserialize)]
pub struct NonceRequest {
    pub wallet: String,
}

#[derive(Debug, Serialize)]
pub struct NonceResponse {
    pub wallet: String,
    pub nonce: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub wallet: String,
    pub signature: String,
    pub nonce: String,
}

#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    pub wallet: String,
    pub ok: bool,
}

pub async fn cleanup_expired_loop(_db: sqlx::SqlitePool) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

pub async fn post_nonce(_req: AppJson<NonceRequest>) -> Result<Json<NonceResponse>, AppError> {
    let wallet = _req.0.wallet;
    Ok(Json(NonceResponse {
        wallet: wallet.clone(),
        nonce: format!("nonce:{wallet}"),
    }))
}

pub async fn post_verify(_req: AppJson<VerifyRequest>) -> Result<Json<VerifyResponse>, AppError> {
    let wallet = _req.0.wallet;
    Ok(Json(VerifyResponse {
        wallet,
        ok: true,
    }))
}

pub async fn get_me(AuthUser(wallet): AuthUser) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(serde_json::json!({ "wallet": wallet })))
}
