use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, SecondsFormat, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;
use sqlx::SqlitePool;

use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

const CHALLENGE_TTL_SECS: i64 = 300;
const MIN_SESSION_TTL_SECS: u64 = 60;
const MAX_SESSION_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const MAX_SERIES_PER_KEY: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOperation {
    Settlement,
    Exercise,
}

impl SessionOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Settlement => "settlement",
            Self::Exercise => "exercise",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionKeyRequest {
    pub operations: Vec<SessionOperation>,
    pub series_ids: Vec<String>,
    pub expires_in_seconds: u64,
    pub spend_asset: String,
    /// Integer quantity in the asset's smallest unit.
    pub spend_limit: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredGrant {
    operations: Vec<SessionOperation>,
    series_ids: Vec<String>,
    expires_at: String,
    spend_asset: String,
    spend_limit: i64,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmSessionKeyRequest {
    pub challenge_id: String,
    /// Base64-encoded Ed25519 wallet signature over the returned challenge.
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct SessionKeyChallengeResponse {
    pub challenge_id: String,
    pub challenge: String,
    pub session_key_id: String,
    pub session_public_key: String,
    pub challenge_expires_at: String,
    pub expires_at: String,
}

#[derive(Debug, Serialize)]
pub struct SessionKeyResponse {
    pub id: String,
    pub public_key: String,
    pub operations: Vec<SessionOperation>,
    pub series_ids: Vec<String>,
    pub spend_asset: String,
    pub spend_limit: i64,
    pub spent_amount: i64,
    pub expires_at: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiFailure {
    error: &'static str,
    code: &'static str,
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let status = match self.code {
            "UNAUTHENTICATED" => StatusCode::UNAUTHORIZED,
            "INVALID_CREDENTIALS" => StatusCode::UNAUTHORIZED,
            "INVALID_REQUEST" => StatusCode::BAD_REQUEST,
            "FORBIDDEN" => StatusCode::FORBIDDEN,
            "NOT_FOUND" => StatusCode::NOT_FOUND,
            "CONFLICT" => StatusCode::CONFLICT,
            "SERVICE_UNAVAILABLE" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

fn failure(error: &'static str, code: &'static str) -> ApiFailure {
    ApiFailure { error, code }
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

async fn authenticated_wallet(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<String, ApiFailure> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| failure("A valid bearer session is required.", "UNAUTHENTICATED"))?;
    let now = now_string();
    sqlx::query_scalar::<_, String>(
        "SELECT wallet_address FROM sessions WHERE token = ? AND expires_at > ?",
    )
    .bind(token)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to resolve session-key owner");
        failure("Could not authenticate the session.", "INTERNAL_ERROR")
    })?
    .ok_or_else(|| failure("The bearer session is invalid or expired.", "UNAUTHENTICATED"))
}

fn master_secret() -> Result<SecretString, ApiFailure> {
    let secret = std::env::var("ZENITH_SESSION_KEY_MASTER").map_err(|_| {
        failure(
            "Session-key signing is not configured.",
            "SERVICE_UNAVAILABLE",
        )
    })?;
    let secret = SecretString::from(secret);
    if secret.expose_secret().as_bytes().len() < 32 {
        return Err(failure(
            "Session-key signing configuration is invalid.",
            "SERVICE_UNAVAILABLE",
        ));
    }
    Ok(secret)
}

fn derive_signing_key(session_key_id: &str) -> Result<ed25519_dalek::SigningKey, ApiFailure> {
    let secret = master_secret()?;
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|_| failure("Session-key signing is unavailable.", "SERVICE_UNAVAILABLE"))?;
    mac.update(b"zenith-session-key-ed25519-v1:");
    mac.update(session_key_id.as_bytes());
    let seed: [u8; 32] = mac.finalize().into_bytes().into();
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

fn verify_wallet_signature(wallet: &str, challenge: &str, encoded_signature: &str) -> Result<(), ApiFailure> {
    let signature = data_encoding::BASE64
        .decode(encoded_signature.as_bytes())
        .map_err(|_| failure("Wallet signature is malformed.", "INVALID_REQUEST"))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| failure("Wallet signature is malformed.", "INVALID_REQUEST"))?;
    let public_key = crate::strkey::decode_stellar_public_key(wallet)
        .map_err(|_| failure("Wallet address is invalid.", "INVALID_CREDENTIALS"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|_| failure("Wallet address is invalid.", "INVALID_CREDENTIALS"))?;
    verifying_key
        .verify_strict(challenge.as_bytes(), &signature)
        .map_err(|_| failure("Wallet authorization signature is invalid.", "INVALID_CREDENTIALS"))
}

fn canonical_challenge(
    wallet: &str,
    session_key_id: &str,
    public_key: &str,
    nonce: &str,
    grant: &StoredGrant,
) -> String {
    let operations = grant
        .operations
        .iter()
        .map(|operation| operation.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let series = grant.series_ids.join(",");
    format!(
        "Zenith session key authorization v1\nwallet={wallet}\nsession_key_id={session_key_id}\npublic_key={public_key}\noperations={operations}\nseries_ids={series}\nexpires_at={}\nspend_asset={}\nspend_limit={}\nnonce={nonce}",
        grant.expires_at, grant.spend_asset, grant.spend_limit
    )
}

fn validate_request(request: &mut CreateSessionKeyRequest) -> Result<(), ApiFailure> {
    if request.operations.is_empty()
        || request.series_ids.is_empty()
        || request.series_ids.len() > MAX_SERIES_PER_KEY
        || request.spend_limit <= 0
        || request.spend_asset.trim().is_empty()
        || request.spend_asset.len() > 64
        || !(MIN_SESSION_TTL_SECS..=MAX_SESSION_TTL_SECS)
            .contains(&request.expires_in_seconds)
    {
        return Err(failure(
            "Session-key scopes, expiry, asset, or spend limit are invalid.",
            "INVALID_REQUEST",
        ));
    }
    request.series_ids.sort();
    request.series_ids.dedup();
    if request.series_ids.is_empty()
        || request.series_ids.len() > MAX_SERIES_PER_KEY
        || request.series_ids.iter().any(|series| {
            series.is_empty()
                || series.len() > 128
                || !series
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
        })
    {
        return Err(failure(
            "A session key must be scoped to 1 to 50 valid, unique series identifiers.",
            "INVALID_REQUEST",
        ));
    }
    request.operations.sort_by_key(|operation| operation.as_str());
    request.operations.dedup();
    if !request
        .spend_asset
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == ':' || character == '-')
    {
        return Err(failure("The spend asset identifier is invalid.", "INVALID_REQUEST"));
    }
    Ok(())
}

pub async fn create_challenge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<CreateSessionKeyRequest>,
) -> Result<Json<SessionKeyChallengeResponse>, ApiFailure> {
    let wallet = authenticated_wallet(&state.db, &headers).await?;
    validate_request(&mut request)?;
    let session_key_id = uuid::Uuid::new_v4().to_string();
    let challenge_id = uuid::Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + Duration::seconds(CHALLENGE_TTL_SECS))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let session_expires_at = (Utc::now() + Duration::seconds(request.expires_in_seconds as i64))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let grant = StoredGrant {
        operations: request.operations,
        series_ids: request.series_ids,
        expires_at: session_expires_at,
        spend_asset: request.spend_asset,
        spend_limit: request.spend_limit,
    };
    let signing_key = derive_signing_key(&session_key_id)?;
    let public_key = crate::strkey::encode_stellar_public_key(
        signing_key.verifying_key().as_bytes(),
    );
    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    let nonce = data_encoding::BASE64URL_NOPAD.encode(&nonce);
    let challenge = canonical_challenge(
        &wallet,
        &session_key_id,
        &public_key,
        &nonce,
        &grant,
    );
    let grant_json = serde_json::to_string(&grant).map_err(|error| {
        tracing::error!(%error, "failed to serialize session-key grant");
        failure("Could not create a session-key challenge.", "INTERNAL_ERROR")
    })?;

    sqlx::query("DELETE FROM session_key_challenges WHERE expires_at <= ?")
        .bind(now_string())
        .execute(&state.db)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to clean up expired session-key challenges");
            failure("Could not create a session-key challenge.", "INTERNAL_ERROR")
        })?;
    sqlx::query(
        "INSERT INTO session_key_challenges \
         (id, wallet_address, session_key_id, session_public_key, challenge, grant_json, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&challenge_id)
    .bind(&wallet)
    .bind(&session_key_id)
    .bind(&public_key)
    .bind(&challenge)
    .bind(grant_json)
    .bind(&expires_at)
    .execute(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to store session-key challenge");
        failure("Could not create a session-key challenge.", "INTERNAL_ERROR")
    })?;

    Ok(Json(SessionKeyChallengeResponse {
        challenge_id,
        challenge,
        session_key_id,
        session_public_key: public_key,
        challenge_expires_at: expires_at,
        expires_at: grant.expires_at,
    }))
}

pub async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ConfirmSessionKeyRequest>,
) -> Result<(StatusCode, Json<SessionKeyResponse>), ApiFailure> {
    let wallet = authenticated_wallet(&state.db, &headers).await?;
    let now = now_string();
    let challenge: Option<(String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT wallet_address, session_key_id, session_public_key, challenge, grant_json, expires_at \
         FROM session_key_challenges WHERE id = ?",
    )
    .bind(&request.challenge_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to load session-key challenge");
        failure("Could not verify the session-key challenge.", "INTERNAL_ERROR")
    })?;
    let Some((owner, key_id, expected_public_key, challenge, grant_json, challenge_expires_at)) =
        challenge
    else {
        return Err(failure("Session-key challenge not found.", "NOT_FOUND"));
    };
    if owner != wallet {
        return Err(failure("Session-key challenge not found.", "NOT_FOUND"));
    }
    if challenge_expires_at <= now {
        return Err(failure("Session-key challenge has expired.", "CONFLICT"));
    }

    verify_wallet_signature(&wallet, &challenge, &request.signature)?;

    let grant: StoredGrant = serde_json::from_str(&grant_json).map_err(|error| {
        tracing::error!(%error, "stored session-key grant is malformed");
        failure("Could not verify the session-key grant.", "INTERNAL_ERROR")
    })?;
    let signing_key = derive_signing_key(&key_id)?;
    let session_public_key = crate::strkey::encode_stellar_public_key(
        signing_key.verifying_key().as_bytes(),
    );
    if session_public_key != expected_public_key {
        return Err(failure(
            "Session-key signing configuration changed; request a new challenge.",
            "CONFLICT",
        ));
    }
    let mut transaction = state.db.begin().await.map_err(|error| {
        tracing::error!(%error, "failed to begin session-key registration");
        failure("Could not register the session key.", "INTERNAL_ERROR")
    })?;
    sqlx::query(
        "INSERT INTO session_keys \
         (id, wallet_address, public_key, operations_json, series_ids_json, spend_asset, \
          spend_limit, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key_id)
    .bind(&wallet)
    .bind(&session_public_key)
    .bind(serde_json::to_string(&grant.operations).map_err(|error| {
        tracing::error!(%error, "failed to serialize session-key operations");
        failure("Could not register the session key.", "INTERNAL_ERROR")
    })?)
    .bind(serde_json::to_string(&grant.series_ids).map_err(|error| {
        tracing::error!(%error, "failed to serialize session-key series");
        failure("Could not register the session key.", "INTERNAL_ERROR")
    })?)
    .bind(&grant.spend_asset)
    .bind(grant.spend_limit)
    .bind(&grant.expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to register session key");
        failure("Could not register the session key.", "INTERNAL_ERROR")
    })?;
    let deleted = sqlx::query(
        "DELETE FROM session_key_challenges WHERE id = ? AND wallet_address = ?",
    )
    .bind(&request.challenge_id)
    .bind(&wallet)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to consume session-key challenge");
        failure("Could not register the session key.", "INTERNAL_ERROR")
    })?;
    if deleted.rows_affected() != 1 {
        return Err(failure("Session-key challenge has already been used.", "CONFLICT"));
    }
    transaction.commit().await.map_err(|error| {
        tracing::error!(%error, "failed to commit session-key registration");
        failure("Could not register the session key.", "INTERNAL_ERROR")
    })?;

    Ok((
        StatusCode::CREATED,
        Json(SessionKeyResponse {
            id: key_id,
            public_key: session_public_key,
            operations: grant.operations,
            series_ids: grant.series_ids,
            spend_asset: grant.spend_asset,
            spend_limit: grant.spend_limit,
            spent_amount: 0,
            expires_at: grant.expires_at,
            created_at: now,
            revoked_at: None,
        }),
    ))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SessionKeyResponse>>, ApiFailure> {
    let wallet = authenticated_wallet(&state.db, &headers).await?;
    let rows: Vec<(String, String, String, String, String, i64, i64, String, String, Option<String>)> =
        sqlx::query_as(
            "SELECT id, public_key, operations_json, series_ids_json, spend_asset, \
             spend_limit, spent_amount, expires_at, created_at, revoked_at \
             FROM session_keys WHERE wallet_address = ? ORDER BY created_at DESC",
        )
        .bind(wallet)
        .fetch_all(&state.db)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to list session keys");
            failure("Could not list session keys.", "INTERNAL_ERROR")
        })?;

    let items = rows
        .into_iter()
        .map(
            |(
                id,
                public_key,
                operations_json,
                series_ids_json,
                spend_asset,
                spend_limit,
                spent_amount,
                expires_at,
                created_at,
                revoked_at,
            )| {
                Ok(SessionKeyResponse {
                    id,
                    public_key,
                    operations: serde_json::from_str(&operations_json).map_err(|error| {
                        tracing::error!(%error, "stored session-key operations are malformed");
                        failure("Stored session-key data is invalid.", "INTERNAL_ERROR")
                    })?,
                    series_ids: serde_json::from_str(&series_ids_json).map_err(|error| {
                        tracing::error!(%error, "stored session-key series are malformed");
                        failure("Stored session-key data is invalid.", "INTERNAL_ERROR")
                    })?,
                    spend_asset,
                    spend_limit,
                    spent_amount,
                    expires_at,
                    created_at,
                    revoked_at,
                })
            },
        )
        .collect::<Result<Vec<_>, ApiFailure>>()?;
    Ok(Json(items))
}

pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let wallet = authenticated_wallet(&state.db, &headers).await?;
    let result = sqlx::query(
        "UPDATE session_keys SET revoked_at = ? \
         WHERE id = ? AND wallet_address = ? AND revoked_at IS NULL",
    )
    .bind(now_string())
    .bind(key_id)
    .bind(wallet)
    .execute(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to revoke session key");
        failure("Could not revoke the session key.", "INTERNAL_ERROR")
    })?;
    if result.rows_affected() != 1 {
        return Err(failure("Session key not found.", "NOT_FOUND"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Sign a transaction hash from a trusted settlement/exercise builder after
/// validating its declared operation, series and asset spend against the
/// registered grant. Spend is reserved atomically before signing, so concurrent
/// requests cannot overspend the key.
pub(crate) async fn sign_scoped_transaction(
    pool: &SqlitePool,
    wallet: &str,
    key_id: &str,
    operation: SessionOperation,
    series_id: &str,
    spend_asset: &str,
    spend_amount: i64,
    transaction_hash: &[u8],
) -> Result<Vec<u8>, ApiFailure> {
    if spend_amount < 0 || transaction_hash.len() != 32 {
        return Err(failure("Transaction signing request is invalid.", "INVALID_REQUEST"));
    }
    let now = now_string();
    let grant: Option<(String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT public_key, operations_json, series_ids_json, spend_asset, spend_limit, spent_amount \
         FROM session_keys WHERE id = ? AND wallet_address = ? \
         AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(key_id)
    .bind(wallet)
    .bind(&now)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to load session-key signing grant");
        failure("Could not authorize transaction signing.", "INTERNAL_ERROR")
    })?
    .ok_or_else(|| failure("Session key is missing, expired, or revoked.", "UNAUTHENTICATED"))?;
    let (expected_public_key, operations_json, series_json, grant_asset, spend_limit, _) = grant;
    let operations: Vec<SessionOperation> = serde_json::from_str(&operations_json)
        .map_err(|_| failure("Stored session-key scope is invalid.", "INTERNAL_ERROR"))?;
    let series_ids: Vec<String> = serde_json::from_str(&series_json)
        .map_err(|_| failure("Stored session-key scope is invalid.", "INTERNAL_ERROR"))?;
    if !operations.contains(&operation) || !series_ids.iter().any(|series| series == series_id) {
        return Err(failure(
            "Session key is not authorized for this operation or series.",
            "FORBIDDEN",
        ));
    }
    if spend_asset != grant_asset || spend_amount > spend_limit {
        return Err(failure(
            "Transaction exceeds the session key's asset or spend ceiling.",
            "FORBIDDEN",
        ));
    }

    let signing_key = derive_signing_key(key_id)?;
    if crate::strkey::encode_stellar_public_key(signing_key.verifying_key().as_bytes())
        != expected_public_key
    {
        return Err(failure(
            "Session-key signing configuration changed; contact the operator.",
            "SERVICE_UNAVAILABLE",
        ));
    }
    let result = sqlx::query(
        "UPDATE session_keys SET spent_amount = spent_amount + ? \
         WHERE id = ? AND wallet_address = ? AND revoked_at IS NULL AND expires_at > ? \
         AND spent_amount <= spend_limit - ?",
    )
    .bind(spend_amount)
    .bind(key_id)
    .bind(wallet)
    .bind(now)
    .bind(spend_amount)
    .execute(pool)
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to reserve session-key spend");
        failure("Could not authorize transaction signing.", "INTERNAL_ERROR")
    })?;
    if result.rows_affected() != 1 {
        return Err(failure(
            "Session key has expired, been revoked, or exhausted its spend ceiling.",
            "FORBIDDEN",
        ));
    }

    use ed25519_dalek::Signer as _;
    Ok(signing_key.sign(transaction_hash).to_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_challenge_binds_operation_series_expiry_and_spend() {
        let grant = StoredGrant {
            operations: vec![SessionOperation::Exercise],
            series_ids: vec!["series-1".into()],
            expires_at: "2026-10-22T00:00:00.000Z".into(),
            spend_asset: "USDC".into(),
            spend_limit: 10_000,
        };
        let message = canonical_challenge(
            "GABC",
            "key-1",
            "GKEY",
            "nonce",
            &grant,
        );
        assert!(message.contains("operations=exercise"));
        assert!(message.contains("series_ids=series-1"));
        assert!(message.contains("spend_limit=10000"));
    }

    #[test]
    fn rejects_unbounded_or_empty_grants() {
        let mut invalid = CreateSessionKeyRequest {
            operations: vec![SessionOperation::Exercise],
            series_ids: vec!["series-1".into()],
            expires_in_seconds: MAX_SESSION_TTL_SECS + 1,
            spend_asset: "USDC".into(),
            spend_limit: 1,
        };
        assert!(validate_request(&mut invalid).is_err());
        invalid.expires_in_seconds = MIN_SESSION_TTL_SECS;
        invalid.series_ids.clear();
        assert!(validate_request(&mut invalid).is_err());
    }

    #[test]
    fn verifies_authorization_with_the_authenticated_wallet_key() {
        use ed25519_dalek::Signer as _;
        let wallet_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let wallet = crate::strkey::encode_stellar_public_key(
            wallet_key.verifying_key().as_bytes(),
        );
        let signature = wallet_key.sign(b"challenge").to_bytes();
        let encoded = data_encoding::BASE64.encode(&signature);
        assert!(verify_wallet_signature(&wallet, "challenge", &encoded).is_ok());
        assert!(verify_wallet_signature(&wallet, "modified", &encoded).is_err());
    }
}
