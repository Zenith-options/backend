use axum::extract::{FromRequestParts, State};
use axum::http::{request::Parts, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use data_encoding::BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::net::IpAddr;
use validator::Validate;

use crate::error::{db_error, AppError, ValidatedJson};
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

#[derive(Deserialize, Validate)]
pub struct NonceRequest {
    #[validate(length(min = 1, max = 64))]
    pub wallet_address: String,
}

#[derive(Serialize)]
pub struct NonceResponse {
    pub nonce: String,
    pub message: String,
}

pub async fn post_nonce(
    State(state): State<AppState>,
    ValidatedJson(req): ValidatedJson<NonceRequest>,
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

#[derive(Deserialize, Validate)]
pub struct VerifyRequest {
    #[validate(length(min = 1, max = 64))]
    pub wallet_address: String,
    #[validate(length(min = 1, max = 128))]
    pub message: String,
    #[validate(length(min = 1, max = 128))]
    pub signature: String, // base64-encoded 64-byte ed25519 signature
}

#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    pub token: String,
    pub wallet_address: String,
}

pub async fn post_verify(
    State(state): State<AppState>,
    ValidatedJson(req): ValidatedJson<VerifyRequest>,
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

    let pubkey_bytes =
        crate::strkey::decode_stellar_public_key(&req.wallet_address).map_err(|_| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "wallet_address is not a valid Stellar G... address",
            )
        })?;
    let verifying_key = VerifyingKey::from_bytes(&pubkey_bytes).map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "wallet_address decodes to an invalid ed25519 key",
        )
    })?;

    let sig_bytes = BASE64
        .decode(req.signature.as_bytes())
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "signature is not valid base64"))?;
    let sig_array: [u8; 64] = sig_bytes.try_into().map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "signature must be exactly 64 bytes",
        )
    })?;
    let signature = Signature::from_bytes(&sig_array);

    verifying_key
        .verify_strict(req.message.as_bytes(), &signature)
        .map_err(|_| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                "signature does not verify against wallet_address for this message",
            )
        })?;

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
        if let Some(identity) = parts.extensions.get::<ApiKeyIdentity>() {
            return Ok(AuthUser(identity.wallet_address.clone()));
        }
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

#[derive(Clone)]
        pub struct ApiKeyIdentity {
            pub wallet_address: String,
            pub scopes: Vec<String>,
        }

        #[derive(FromRow)]
        struct ApiKeyRow {
            wallet_address: String,
            secret: String,
            scopes: String,
            ip_allowlist: Option<String>,
            expires_at: Option<String>,
        }

        type HmacSha256 = Hmac<Sha256>;

        /// Authenticates optional API-key-signed requests. Session-based wallet
        /// authentication remains available for interactive clients; API keys are
        /// never accepted as bearer tokens.
        pub async fn api_key_middleware(
            State(state): State<AppState>,
            request: Request<axum::body::Body>,
            next: Next,
        ) -> Response {
            let key_id_header = request.headers().get("x-api-key");
            if key_id_header.is_none() {
                if request.headers().contains_key("x-api-timestamp")
                    || request.headers().contains_key("x-api-signature")
                {
                    return AppError::new(StatusCode::UNAUTHORIZED, "incomplete API request signature").into_response();
                }
                return next.run(request).await;
            }
            let Some(key_id) = key_id_header.and_then(|v| v.to_str().ok()) else {
                return AppError::new(StatusCode::UNAUTHORIZED, "invalid API key ID").into_response();
            };
            let reject = |status, message: &'static str| {
                AppError::new(status, message).into_response()
            };
            let now = now_unix();
            let timestamp = match request
                .headers()
                .get("x-api-timestamp")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
            {
                Some(timestamp) if timestamp.abs_diff(now) <= 300 => timestamp,
                _ => return reject(StatusCode::UNAUTHORIZED, "missing or expired API signature timestamp"),
            };
            let signature = match request.headers().get("x-api-signature").and_then(|v| v.to_str().ok()).map(str::to_owned) {
                Some(signature) => signature,
                None => return reject(StatusCode::UNAUTHORIZED, "missing API request signature"),
            };
            let row: Option<ApiKeyRow> = match sqlx::query_as(
                "SELECT wallet_address, secret, scopes, ip_allowlist, expires_at FROM api_keys WHERE id = ?",
            )
            .bind(key_id)
            .fetch_optional(&state.db)
            .await
            {
                Ok(row) => row,
                Err(error) => {
                    tracing::error!(%error, "failed to load API key");
                    return AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "failed to authenticate API key").into_response();
                }
            };
            let Some(row) = row else {
                return reject(StatusCode::UNAUTHORIZED, "unknown API key");
            };
            if row.expires_at.as_deref().is_some_and(|expiry| expiry < format_unix_secs(now_unix()).as_str()) {
                return reject(StatusCode::UNAUTHORIZED, "API key expired");
            }
            if let Some(allowlist) = row.ip_allowlist.as_deref() {
                let peer_ip = request.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                    .map(|connect| connect.0.ip());
                let allowlist = match serde_json::from_str::<Vec<String>>(allowlist) {
                    Ok(ips) => ips,
                    Err(error) => {
                        tracing::error!(%error, "stored API key IP allowlist is invalid");
                        return AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "failed to authenticate API key").into_response();
                    }
                };
                let allowed = peer_ip.is_some_and(|ip| {
                    allowlist.iter().any(|candidate| candidate.parse::<IpAddr>() == Ok(ip))
                });
                if !allowed {
                    return reject(StatusCode::FORBIDDEN, "request IP is not allowlisted");
                }
            }
            let (parts, body) = request.into_parts();
            let body = match axum::body::to_bytes(body, 2 * 1024 * 1024).await {
                Ok(body) => body,
                Err(_) => return reject(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
            };
            let body_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&body));
            let path = parts.uri.path_and_query().map(|v| v.as_str()).unwrap_or("/");
            let canonical = format!("{}\n{}\n{}\n{}", parts.method, path, timestamp, body_hash);
            let Ok(secret) = data_encoding::HEXLOWER.decode(row.secret.as_bytes()) else {
                return reject(StatusCode::UNAUTHORIZED, "invalid API key");
            };
            let Ok(mut mac) = HmacSha256::new_from_slice(&secret) else {
                return reject(StatusCode::UNAUTHORIZED, "invalid API key");
            };
            mac.update(canonical.as_bytes());
            let Ok(signature_bytes) = data_encoding::HEXLOWER.decode(signature.as_bytes()) else {
                return reject(StatusCode::UNAUTHORIZED, "invalid API request signature");
            };
            if mac.verify_slice(&signature_bytes).is_err() {
                return reject(StatusCode::UNAUTHORIZED, "invalid API request signature");
            }
            let scopes: Vec<String> = match serde_json::from_str(&row.scopes) {
                Ok(scopes) => scopes,
                Err(error) => {
                    tracing::error!(%error, "stored API key scopes are invalid");
                    return AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "failed to authenticate API key").into_response();
                }
            };
            let required_scope = if path.starts_with("/api/v1/alerts") {
                "alerts"
            } else if path.starts_with("/api/v1/auth/keys") {
                return reject(StatusCode::FORBIDDEN, "API keys cannot manage API keys");
            } else if path.starts_with("/api/v1/price/batch")
                || path.starts_with("/api/v1/portfolio/payoff")
            {
                "read"
            } else if parts.method == axum::http::Method::GET {
                "read"
            } else {
                "trade"
            };
            if !scopes.iter().any(|scope| scope == required_scope) {
                return reject(StatusCode::FORBIDDEN, "API key lacks the required scope");
            }
            let mut request = Request::from_parts(parts, axum::body::Body::from(body));
            request.extensions_mut().insert(ApiKeyIdentity {
                wallet_address: row.wallet_address,
                scopes,
            });
            next.run(request).await
        }

        #[derive(Deserialize, Validate)]
        pub struct CreateApiKeyRequest {
            #[validate(length(min = 1, max = 64))]
            pub label: String,
            #[validate(length(min = 1, max = 3))]
            #[validate(custom(function = "validate_scopes"))]
            pub scopes: Vec<String>,
            #[validate(custom(function = "validate_allowlist"))]
            pub ip_allowlist: Option<Vec<String>>,
            #[validate(custom(function = "validate_expiry"))]
            pub expires_at: Option<String>,
        }

        fn validate_scopes(scopes: &[String]) -> Result<(), validator::ValidationError> {
            if scopes.iter().all(|scope| ["read", "trade", "alerts"].contains(&scope.as_str()))
                && scopes.iter().collect::<std::collections::HashSet<_>>().len() == scopes.len()
            {
                Ok(())
            } else {
                Err(validator::ValidationError::new("invalid_scope"))
            }
        }

        fn validate_allowlist(ips: &Vec<String>) -> Result<(), validator::ValidationError> {
            if ips.len() <= 50 && ips.iter().all(|ip| ip.parse::<IpAddr>().is_ok()) {
                Ok(())
            } else {
                Err(validator::ValidationError::new("invalid_ip_allowlist"))
            }
        }

        fn validate_expiry(value: &String) -> Result<(), validator::ValidationError> {
            if chrono::DateTime::parse_from_rfc3339(value)
                .is_ok_and(|date| date > chrono::Utc::now())
            {
                Ok(())
            } else {
                Err(validator::ValidationError::new("invalid_expiry"))
            }
        }

        #[derive(Serialize)]
        pub struct CreateApiKeyResponse {
            pub id: String,
            pub secret: String,
            pub label: String,
            pub scopes: Vec<String>,
            pub ip_allowlist: Option<Vec<String>>,
            pub expires_at: Option<String>,
        }

        pub async fn create_api_key(
            State(state): State<AppState>,
            AuthUser(wallet_address): AuthUser,
            identity: Option<axum::extract::Extension<ApiKeyIdentity>>,
            ValidatedJson(req): ValidatedJson<CreateApiKeyRequest>,
        ) -> Result<(StatusCode, Json<CreateApiKeyResponse>), AppError> {
            if identity.is_some() {
                return Err(AppError::new(StatusCode::FORBIDDEN, "API keys cannot create API keys"));
            }
            let id = uuid::Uuid::new_v4().to_string();
            let secret = random_token_hex(32);
            sqlx::query("INSERT INTO api_keys (id, wallet_address, secret, scopes, ip_allowlist, expires_at, label) VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(&id)
                .bind(&wallet_address)
                .bind(&secret)
                .bind(serde_json::to_string(&req.scopes).unwrap())
                .bind(req.ip_allowlist.as_ref().map(serde_json::to_string).transpose().map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "failed to encode API key allowlist"))?)
                .bind(req.expires_at.as_ref().map(|value| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .expect("validated RFC3339 expiry")
                        .with_timezone(&chrono::Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                }))
                .bind(&req.label)
                .execute(&state.db)
                .await
                .map_err(|e| db_error("create API key", e))?;
            Ok((StatusCode::CREATED, Json(CreateApiKeyResponse {
                id, secret, label: req.label, scopes: req.scopes, ip_allowlist: req.ip_allowlist, expires_at: req.expires_at,
            })))
        }

        pub async fn delete_api_key(
            State(state): State<AppState>,
            AuthUser(wallet_address): AuthUser,
            axum::extract::Path(id): axum::extract::Path<String>,
        ) -> Result<StatusCode, AppError> {
            let result = sqlx::query("DELETE FROM api_keys WHERE id = ? AND wallet_address = ?")
                .bind(id)
                .bind(wallet_address)
                .execute(&state.db)
                .await
                .map_err(|e| db_error("delete API key", e))?;
            if result.rows_affected() == 0 {
                return Err(AppError::new(StatusCode::NOT_FOUND, "API key not found"));
            }
            Ok(StatusCode::NO_CONTENT)
        }

pub async fn get_me(auth: AuthUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "wallet_address": auth.0 }))
}

/// Deletes every expired nonce/session row as of now. Returns
/// (expired_nonces, expired_sessions) removed, or an sqlx error from
/// whichever DELETE ran into one — pulled out of the loop below so it has
/// a return value the loop can log and tests can assert on directly,
/// instead of only being observable through log lines or side effects on
/// a live timer.
pub async fn sweep_expired(db: &sqlx::SqlitePool) -> Result<(u64, u64), sqlx::Error> {
    let now = format_unix_secs(now_unix());

    let nonces = sqlx::query("DELETE FROM auth_nonces WHERE expires_at < ?")
        .bind(&now)
        .execute(db)
        .await?;
    let sessions = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(&now)
        .execute(db)
        .await?;

    Ok((nonces.rows_affected(), sessions.rows_affected()))
}

/// Sweeps expired nonces and sessions every 5 minutes. Neither table is
/// large or hot enough to need anything fancier than a periodic DELETE;
/// this just keeps them from growing forever.
pub async fn cleanup_expired_loop(db: sqlx::SqlitePool) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
    loop {
        interval.tick().await;

        match sweep_expired(&db).await {
            Ok((n, s)) if n > 0 || s > 0 => {
                tracing::info!(
                    expired_nonces = n,
                    expired_sessions = s,
                    "swept expired auth rows"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "auth cleanup sweep failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> (sqlx::SqlitePool, std::path::PathBuf) {
        let db_path =
            std::env::temp_dir().join(format!("zenith-auth-test-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", db_path.display())).await;
        (pool, db_path)
    }

    #[tokio::test]
    async fn sweep_expired_removes_only_expired_rows() {
        let (db, db_path) = test_db().await;

        let past = format_unix_secs(now_unix() - 3600);
        let future = format_unix_secs(now_unix() + 3600);

        sqlx::query(
            "INSERT INTO auth_nonces (nonce, wallet_address, expires_at) VALUES (?, 'GTEST', ?)",
        )
        .bind("expired-nonce")
        .bind(&past)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auth_nonces (nonce, wallet_address, expires_at) VALUES (?, 'GTEST', ?)",
        )
        .bind("live-nonce")
        .bind(&future)
        .execute(&db)
        .await
        .unwrap();

        sqlx::query("INSERT INTO accounts (wallet_address) VALUES ('GTEST')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions (token, wallet_address, expires_at) VALUES (?, 'GTEST', ?)",
        )
        .bind("expired-session")
        .bind(&past)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (token, wallet_address, expires_at) VALUES (?, 'GTEST', ?)",
        )
        .bind("live-session")
        .bind(&future)
        .execute(&db)
        .await
        .unwrap();

        let (expired_nonces, expired_sessions) = sweep_expired(&db).await.unwrap();
        assert_eq!(expired_nonces, 1);
        assert_eq!(expired_sessions, 1);

        let remaining_nonces: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_nonces")
            .fetch_one(&db)
            .await
            .unwrap();
        let remaining_sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(remaining_nonces, 1);
        assert_eq!(remaining_sessions, 1);

        db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn sweep_expired_is_a_no_op_when_nothing_has_expired() {
        let (db, db_path) = test_db().await;

        let future = format_unix_secs(now_unix() + 3600);
        sqlx::query("INSERT INTO auth_nonces (nonce, wallet_address, expires_at) VALUES ('live', 'GTEST', ?)")
            .bind(&future)
            .execute(&db)
            .await
            .unwrap();

        let (expired_nonces, expired_sessions) = sweep_expired(&db).await.unwrap();
        assert_eq!(expired_nonces, 0);
        assert_eq!(expired_sessions, 0);

        db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    /// Not reachable through the integration test harness at all — the
    /// real SESSION_TTL_SECS is 24 hours and nothing in this app lets a
    /// caller fast-forward the system clock, so the only way to actually
    /// exercise this specific rejection (found, but expired — distinct
    /// from "garbage/unknown token" in tests/auth_test.rs) is to insert an
    /// already-expired session directly and call the extractor as a plain
    /// function, bypassing the router entirely.
    #[tokio::test]
    async fn auth_user_rejects_a_session_that_has_expired() {
        let (db, db_path) = test_db().await;
        let state = crate::AppState::new(db);

        sqlx::query("INSERT INTO accounts (wallet_address) VALUES ('GTEST')")
            .execute(&state.db)
            .await
            .unwrap();
        let past = format_unix_secs(now_unix() - 3600);
        sqlx::query("INSERT INTO sessions (token, wallet_address, expires_at) VALUES ('tok123', 'GTEST', ?)")
            .bind(&past)
            .execute(&state.db)
            .await
            .unwrap();

        let req = axum::http::Request::builder()
            .header("authorization", "Bearer tok123")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();

        let result = AuthUser::from_request_parts(&mut parts, &state).await;
        let err = result.expect_err("an expired session must not authenticate");
        assert_eq!(err.message, "session expired");

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    /// Same situation as the expired-session test above: NONCE_TTL_SECS is
    /// a real 5 minutes, nothing lets a test fast-forward the clock, so
    /// this inserts an already-expired nonce directly and calls
    /// post_verify as a plain function. The nonce-expiry check runs
    /// before any signature verification, so a throwaway signature that
    /// was never going to be checked is fine here.
    #[tokio::test]
    async fn post_verify_rejects_an_expired_nonce() {
        let (db, db_path) = test_db().await;
        let state = crate::AppState::new(db);

        let message = "Sign in to Zenith\nNonce: deadbeef";
        let past = format_unix_secs(now_unix() - 3600);
        sqlx::query(
            "INSERT INTO auth_nonces (nonce, wallet_address, expires_at) VALUES (?, 'GTEST', ?)",
        )
        .bind(message)
        .bind(&past)
        .execute(&state.db)
        .await
        .unwrap();

        let req = VerifyRequest {
            wallet_address: "GTEST".to_string(),
            message: message.to_string(),
            signature: "AA==".to_string(),
        };
        let result = post_verify(State(state.clone()), ValidatedJson(req)).await;
        let err = result.expect_err("an expired nonce must be rejected");
        assert_eq!(err.message, "nonce expired");

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }
}
