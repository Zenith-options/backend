use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::{request::Parts, StatusCode};
use axum::response::Json;
use data_encoding::BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::error::{db_error, AppError, AppJson};
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
    AppJson(req): AppJson<NonceRequest>,
) -> Result<Json<NonceResponse>, AppError> {
    if crate::strkey::decode_stellar_public_key(&req.wallet_address).is_err() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "wallet_address is not a valid Stellar G... address",
        ));
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
        .map_err(|e| db_error("store auth nonce", e))?;

    Ok(Json(NonceResponse { nonce, message }))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct SignerSignature {
    pub public_key: String,
    pub signature: String, // base64-encoded
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccountSigner {
    pub key: String,
    pub weight: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccountThresholds {
    pub low_threshold: u32,
    pub med_threshold: u32,
    pub high_threshold: u32,
}

impl Default for AccountThresholds {
    fn default() -> Self {
        Self {
            low_threshold: 1,
            med_threshold: 1,
            high_threshold: 1,
        }
    }
}

/// Evaluates if the verified distinct signatures meet the required weight threshold.
/// Master keys with 0 weight are explicitly ignored. Duplicate signatures are only counted once.
pub fn meets_threshold(
    signers: &[AccountSigner],
    threshold: u32,
    valid_signatures: &[SignerSignature],
) -> bool {
    if threshold == 0 {
        return true;
    }

    let mut seen_keys = std::collections::HashSet::new();
    let mut total_weight: u32 = 0;

    for sig in valid_signatures {
        if !seen_keys.insert(&sig.public_key) {
            continue; // Ignore duplicate signatures from same signer
        }

        if let Some(signer) = signers.iter().find(|s| s.key == sig.public_key) {
            if signer.weight > 0 {
                total_weight = total_weight.saturating_add(signer.weight);
            }
        }
    }

    total_weight >= threshold
}

#[derive(Deserialize)]
pub struct VerifyRequest {
    pub wallet_address: String,
    pub message: String,
    pub signature: Option<String>, // base64-encoded 64-byte ed25519 signature (backward-compatible)
    pub signatures: Option<Vec<SignerSignature>>, // multi-signature support
}

#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    pub token: String,
    pub wallet_address: String,
}

pub async fn post_verify(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AppJson(req): AppJson<VerifyRequest>,
) -> Result<Json<VerifyResponse>, AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT expires_at FROM auth_nonces WHERE nonce = ? AND wallet_address = ?")
            .bind(&req.message)
            .bind(&req.wallet_address)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| db_error("look up auth nonce", e))?;

    let (expires_at,) = row.ok_or_else(|| {
        AppError::new(
            StatusCode::UNAUTHORIZED,
            "unknown or already-consumed nonce",
        )
    })?;
    if expires_at.as_str() < format_unix_secs(now_unix()).as_str() {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "nonce expired"));
    }

    // Single-use: consume the nonce regardless of whether the signature
    // below checks out, so a leaked signature can't be replayed either.
    sqlx::query("DELETE FROM auth_nonces WHERE nonce = ?")
        .bind(&req.message)
        .execute(&state.db)
        .await
        .map_err(|e| db_error("consume auth nonce", e))?;

    // Collect signatures to verify
    let mut sig_list: Vec<SignerSignature> = req.signatures.clone().unwrap_or_default();
    if let Some(single_sig) = &req.signature {
        if sig_list.is_empty() {
            sig_list.push(SignerSignature {
                public_key: req.wallet_address.clone(),
                signature: single_sig.clone(),
            });
        }
    }

    if sig_list.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "at least one signature is required",
        ));
    }

    let mut valid_signatures = Vec::new();

    for sig_item in &sig_list {
        let pubkey_bytes = match crate::strkey::decode_stellar_public_key(&sig_item.public_key) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let verifying_key = match VerifyingKey::from_bytes(&pubkey_bytes) {
            Ok(vk) => vk,
            Err(_) => continue,
        };
        let sig_bytes = match BASE64.decode(sig_item.signature.as_bytes()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let sig_array: [u8; 64] = match sig_bytes.try_into() {
            Ok(arr) => arr,
            Err(_) => continue,
        };
        let signature = Signature::from_bytes(&sig_array);

        if verifying_key.verify_strict(req.message.as_bytes(), &signature).is_ok() {
            valid_signatures.push(sig_item.clone());
        }
    }

    // Default signer configuration for single-sig or fallback account
    let default_signers = vec![AccountSigner {
        key: req.wallet_address.clone(),
        weight: 1,
    }];
    let threshold = 1;

    if !meets_threshold(&default_signers, threshold, &valid_signatures) {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "signatures do not meet the required threshold for this wallet",
        ));
    }

    sqlx::query(
        "INSERT INTO accounts (wallet_address) VALUES (?) ON CONFLICT(wallet_address) DO NOTHING",
    )
    .bind(&req.wallet_address)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("create or confirm account", e))?;

    let token = random_token_hex(32);
    let session_expires_at = format_unix_secs(now_unix() + SESSION_TTL_SECS);
    sqlx::query("INSERT INTO sessions (token, wallet_address, expires_at) VALUES (?, ?, ?)")
        .bind(&token)
        .bind(&req.wallet_address)
        .bind(&session_expires_at)
        .execute(&state.db)
        .await
        .map_err(|e| db_error("create session", e))?;

    // Capture the session IP at verify time so the competition service can
    // run sybil heuristics over wallets that share session IPs. Best-effort:
    // a failure to record the IP must not block a valid login.
    let ip = addr.ip().to_string();
    let _ = sqlx::query(
        "INSERT INTO session_ips (wallet_address, ip, session_token, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&req.wallet_address)
    .bind(&ip)
    .bind(&token)
    .bind(format_unix_secs(now_unix()))
    .execute(&state.db)
    .await;

    Ok(Json(VerifyResponse {
        token,
        wallet_address: req.wallet_address,
    }))
}

/// Extractor for routes that require a logged-in wallet. Reads
/// `Authorization: Bearer <token>`, looks it up in `sessions`, and
/// rejects with 401 if missing, unknown, or expired.
#[derive(Debug)]
pub struct AuthUser(pub String);

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let unauthorized =
            || AppError::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");

        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(unauthorized)?;

        let token = header.strip_prefix("Bearer ").ok_or_else(unauthorized)?;

        let row: Option<(String, String)> =
            sqlx::query_as("SELECT wallet_address, expires_at FROM sessions WHERE token = ?")
                .bind(token)
                .fetch_optional(&state.db)
                .await
                .map_err(|e| db_error("look up session", e))?;

        let (wallet_address, expires_at) = row.ok_or_else(unauthorized)?;
        if expires_at.as_str() < format_unix_secs(now_unix()).as_str() {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "session expired"));
        }

        Ok(AuthUser(wallet_address))
    }
}

pub async fn get_me(auth: AuthUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "wallet_address": auth.0 }))
}
