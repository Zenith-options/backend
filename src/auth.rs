use axum::extract::{FromRequestParts, State};
use axum::http::{request::Parts, StatusCode};
use axum::response::Json;
use data_encoding::BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::AppState;

const NONCE_TTL_SECS: i64 = 5 * 60;
const SESSION_TTL_SECS: i64 = 24 * 60 * 60;

fn random_token_hex(len_bytes: usize) -> String {
    let mut bytes = vec![0u8; len_bytes];
    rand::thread_rng().fill_bytes(&mut bytes);
    data_encoding::HEXLOWER.encode(&bytes)
}

/// sqlite's strftime('now') gives us second precision in the schema
/// defaults; we need the same clock in Rust without pulling in chrono
/// just for "now + N seconds" arithmetic, so this uses SystemTime + a
/// tiny hand-rolled RFC3339 formatter (UTC only, which is all we need).
/// Values only ever get compared as strings against each other, so the
/// exact format just needs to sort the same way ISO 8601 does.
fn format_unix_secs(total_secs: i64) -> String {
    // Civil-from-days algorithm (Howard Hinnant's public-domain date
    // algorithms) to avoid a chrono dependency for one timestamp format.
    let days = total_secs.div_euclid(86400);
    let rem = total_secs.rem_euclid(86400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.000Z")
}

#[derive(Deserialize)]
pub struct NonceRequest {
    pub wallet_address: String,
}

#[derive(Serialize)]
pub struct NonceResponse {
    pub nonce: String,
    pub message: String,
}

pub async fn post_nonce(
    State(state): State<AppState>,
    Json(req): Json<NonceRequest>,
) -> Result<Json<NonceResponse>, StatusCode> {
    if crate::strkey::decode_stellar_public_key(&req.wallet_address).is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let nonce = random_token_hex(16);
    let message = format!("Sign in to Zenith\nNonce: {nonce}");
    let expires_at = format_unix_secs(now_unix() + NONCE_TTL_SECS);

    sqlx::query("INSERT INTO auth_nonces (nonce, wallet_address, expires_at) VALUES (?, ?, ?)")
        .bind(&message)
        .bind(&req.wallet_address)
        .bind(&expires_at)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(NonceResponse { nonce, message }))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Deserialize)]
pub struct VerifyRequest {
    pub wallet_address: String,
    pub message: String,
    pub signature: String, // base64-encoded 64-byte ed25519 signature
}

#[derive(Serialize)]
pub struct VerifyResponse {
    pub token: String,
    pub wallet_address: String,
}

pub async fn post_verify(
    State(state): State<AppState>,
    Json(req): Json<VerifyRequest>,
) -> Result<Json<VerifyResponse>, StatusCode> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT expires_at FROM auth_nonces WHERE nonce = ? AND wallet_address = ?",
    )
    .bind(&req.message)
    .bind(&req.wallet_address)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let (expires_at,) = row.ok_or(StatusCode::UNAUTHORIZED)?;
    if expires_at.as_str() < format_unix_secs(now_unix()).as_str() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Single-use: consume the nonce regardless of whether the signature
    // below checks out, so a leaked signature can't be replayed either.
    sqlx::query("DELETE FROM auth_nonces WHERE nonce = ?")
        .bind(&req.message)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let pubkey_bytes = crate::strkey::decode_stellar_public_key(&req.wallet_address)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let verifying_key =
        VerifyingKey::from_bytes(&pubkey_bytes).map_err(|_| StatusCode::BAD_REQUEST)?;

    let sig_bytes = BASE64
        .decode(req.signature.as_bytes())
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let sig_array: [u8; 64] = sig_bytes.try_into().map_err(|_| StatusCode::BAD_REQUEST)?;
    let signature = Signature::from_bytes(&sig_array);

    verifying_key
        .verify_strict(req.message.as_bytes(), &signature)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    sqlx::query(
        "INSERT INTO accounts (wallet_address) VALUES (?) ON CONFLICT(wallet_address) DO NOTHING",
    )
    .bind(&req.wallet_address)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let token = random_token_hex(32);
    let session_expires_at = format_unix_secs(now_unix() + SESSION_TTL_SECS);
    sqlx::query("INSERT INTO sessions (token, wallet_address, expires_at) VALUES (?, ?, ?)")
        .bind(&token)
        .bind(&req.wallet_address)
        .bind(&session_expires_at)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(VerifyResponse { token, wallet_address: req.wallet_address }))
}

/// Extractor for routes that require a logged-in wallet. Reads
/// `Authorization: Bearer <token>`, looks it up in `sessions`, and
/// rejects with 401 if missing, unknown, or expired.
pub struct AuthUser(pub String);

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;

        let token = header
            .strip_prefix("Bearer ")
            .ok_or(StatusCode::UNAUTHORIZED)?;

        let row: Option<(String, String)> =
            sqlx::query_as("SELECT wallet_address, expires_at FROM sessions WHERE token = ?")
                .bind(token)
                .fetch_optional(&state.db)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let (wallet_address, expires_at) = row.ok_or(StatusCode::UNAUTHORIZED)?;
        if expires_at.as_str() < format_unix_secs(now_unix()).as_str() {
            return Err(StatusCode::UNAUTHORIZED);
        }

        Ok(AuthUser(wallet_address))
    }
}

pub async fn get_me(auth: AuthUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "wallet_address": auth.0 }))
}
