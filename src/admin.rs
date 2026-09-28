use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use data_encoding::BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::{
    auth::AuthUser,
    error::{db_error, AppError, AppJson},
    AppState,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole {
    Viewer,
    Operator,
    RiskAdmin,
    SuperAdmin,
}

impl AdminRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::RiskAdmin => "risk_admin",
            Self::SuperAdmin => "super_admin",
        }
    }

    fn level(self) -> i64 {
        match self {
            Self::Viewer => 1,
            Self::Operator => 2,
            Self::RiskAdmin => 3,
            Self::SuperAdmin => 4,
        }
    }
}

pub async fn bootstrap_super_admins(db: &sqlx::SqlitePool) -> Result<(), sqlx::Error> {
    let wallets = std::env::var("ZENITH_SUPER_ADMIN_WALLETS").unwrap_or_default();
    for wallet in wallets.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        sqlx::query(
            "INSERT INTO admin_roles (wallet_address, role)
             VALUES (?, 'super_admin') ON CONFLICT(wallet_address, role) DO NOTHING",
        )
        .bind(wallet)
        .execute(db)
        .await?;
    }
    Ok(())
}

fn forbidden() -> AppError {
    AppError::new(StatusCode::FORBIDDEN, "insufficient admin role")
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, AppError> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token"))
}

async fn require_role(
    state: &AppState,
    wallet: &str,
    headers: &HeaderMap,
    minimum: AdminRole,
    step_up: bool,
) -> Result<(), AppError> {
    let highest_role: Option<String> =
        sqlx::query_scalar("SELECT role FROM admin_roles WHERE wallet_address = ?")
            .bind(wallet)
            .fetch_all(&state.db)
            .await
            .map_err(|e| db_error("look up admin roles", e))?
            .into_iter()
            .max_by_key(|role: &String| match role.as_str() {
                "viewer" => 1,
                "operator" => 2,
                "risk_admin" => 3,
                "super_admin" => 4,
                _ => 0,
            });
    let level = highest_role.as_deref().map(role_level).unwrap_or(0);
    if level < minimum.level() {
        return Err(forbidden());
    }

    if step_up {
        let token = bearer_token(headers)?;
        let valid: i64 = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1 FROM sessions
                 WHERE token = ? AND wallet_address = ?
                   AND admin_step_up_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             )",
        )
        .bind(token)
        .bind(wallet)
        .fetch_one(&state.db)
        .await
        .map_err(|e| db_error("check admin step-up session", e))?;
        if valid == 0 {
            return Err(AppError::new(
                StatusCode::PRECONDITION_REQUIRED,
                "admin step-up authentication required",
            ));
        }
    }
    Ok(())
}

fn role_level(role: &str) -> i64 {
    match role {
        "viewer" => 1,
        "operator" => 2,
        "risk_admin" => 3,
        "super_admin" => 4,
        _ => 0,
    }
}

#[derive(Serialize)]
pub struct StepUpChallenge {
    pub message: String,
    pub expires_in_seconds: u64,
}

pub async fn post_step_up_nonce(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
) -> Result<Json<StepUpChallenge>, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Viewer, false).await?;
    let token = bearer_token(&headers)?;
    let mut random = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut random);
    let nonce = data_encoding::HEXLOWER.encode(&random);
    let message = format!("Zenith admin step-up\nNonce: {nonce}");
    sqlx::query(
        "INSERT INTO admin_step_up_nonces (nonce, wallet_address, session_token, expires_at)
         VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+5 minutes'))",
    )
    .bind(&message)
    .bind(&wallet)
    .bind(token)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("create admin step-up challenge", e))?;

    Ok(Json(StepUpChallenge {
        message,
        expires_in_seconds: 300,
    }))
}

#[derive(Deserialize)]
pub struct StepUpVerifyRequest {
    pub message: String,
    pub signature: String,
}

#[derive(Serialize)]
pub struct StepUpResponse {
    pub step_up_expires_in_seconds: u64,
}

pub async fn post_step_up_verify(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    AppJson(request): AppJson<StepUpVerifyRequest>,
) -> Result<Json<StepUpResponse>, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Viewer, false).await?;
    let token = bearer_token(&headers)?;
    let challenge: Option<(String,)> = sqlx::query_as(
        "SELECT nonce FROM admin_step_up_nonces
         WHERE nonce = ? AND wallet_address = ? AND session_token = ?
           AND expires_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(&request.message)
    .bind(&wallet)
    .bind(token)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| db_error("look up admin step-up challenge", e))?;
    if challenge.is_none() {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "unknown, expired, or consumed admin step-up challenge",
        ));
    }
    let consumed = sqlx::query(
        "DELETE FROM admin_step_up_nonces
         WHERE nonce = ? AND wallet_address = ? AND session_token = ?",
    )
    .bind(&request.message)
    .bind(&wallet)
    .bind(token)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("consume admin step-up challenge", e))?;
    if consumed.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "admin step-up challenge was already consumed",
        ));
    }

    let pubkey = crate::strkey::decode_stellar_public_key(&wallet).map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "wallet_address is not a valid Stellar G... address",
        )
    })?;
    let verifying_key = VerifyingKey::from_bytes(&pubkey).map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "wallet_address decodes to an invalid ed25519 key",
        )
    })?;
    let signature: [u8; 64] = BASE64
        .decode(request.signature.as_bytes())
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "signature is not valid base64"))?
        .try_into()
        .map_err(|_| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "signature must be exactly 64 bytes",
            )
        })?;
    verifying_key
        .verify_strict(
            request.message.as_bytes(),
            &Signature::from_bytes(&signature),
        )
        .map_err(|_| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                "signature does not verify against wallet_address for this challenge",
            )
        })?;

    let result = sqlx::query(
        "UPDATE sessions
         SET admin_step_up_expires_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+10 minutes')
         WHERE token = ? AND wallet_address = ?
           AND expires_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(token)
    .bind(&wallet)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("grant admin step-up session", e))?;
    if result.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "session expired during admin step-up",
        ));
    }
    Ok(Json(StepUpResponse {
        step_up_expires_in_seconds: 600,
    }))
}

#[derive(Serialize)]
pub struct FeatureAdminRow {
    pub name: String,
    pub enabled: bool,
    pub rollout_percent: f64,
    pub allowlisted_wallets: Vec<String>,
}

pub async fn get_admin_features(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
) -> Result<Json<Vec<FeatureAdminRow>>, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Viewer, false).await?;
    let flags: Vec<(String, bool, f64)> = sqlx::query_as(
        "SELECT name, enabled, rollout_percent FROM feature_flags WHERE environment = ? ORDER BY name",
    )
    .bind(state.feature_flags.environment())
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("list admin feature flags", e))?;
    let mut result = Vec::with_capacity(flags.len());
    for (name, enabled, rollout_percent) in flags {
        let wallets: Vec<(String,)> = sqlx::query_as(
            "SELECT wallet_address FROM feature_flag_wallets
             WHERE environment = ? AND flag_name = ? ORDER BY wallet_address",
        )
        .bind(state.feature_flags.environment())
        .bind(&name)
        .fetch_all(&state.db)
        .await
        .map_err(|e| db_error("list feature flag wallet allowlist", e))?;
        result.push(FeatureAdminRow {
            name,
            enabled,
            rollout_percent,
            allowlisted_wallets: wallets.into_iter().map(|(wallet,)| wallet).collect(),
        });
    }
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct UpsertFeatureRequest {
    pub enabled: bool,
    pub rollout_percent: f64,
    #[serde(default)]
    pub allowlisted_wallets: Vec<String>,
}

pub async fn put_admin_feature(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    Path(name): Path<String>,
    AppJson(request): AppJson<UpsertFeatureRequest>,
) -> Result<StatusCode, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Operator, true).await?;
    if name.trim().is_empty()
        || name.len() > 128
        || !request.rollout_percent.is_finite()
        || !(0.0..=100.0).contains(&request.rollout_percent)
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "feature name must be non-empty and rollout_percent must be between 0 and 100",
        ));
    }
    if request
        .allowlisted_wallets
        .iter()
        .any(|wallet| crate::strkey::decode_stellar_public_key(wallet).is_err())
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "allowlisted_wallets must contain valid Stellar G... addresses",
        ));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| db_error("begin feature flag update", e))?;
    sqlx::query(
        "INSERT INTO feature_flags (environment, name, enabled, rollout_percent)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(environment, name) DO UPDATE SET
           enabled = excluded.enabled,
           rollout_percent = excluded.rollout_percent,
           updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(state.feature_flags.environment())
    .bind(&name)
    .bind(request.enabled)
    .bind(request.rollout_percent)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_error("save feature flag", e))?;
    sqlx::query("DELETE FROM feature_flag_wallets WHERE environment = ? AND flag_name = ?")
        .bind(state.feature_flags.environment())
        .bind(&name)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_error("replace feature flag allowlist", e))?;
    for allowed_wallet in request.allowlisted_wallets {
        sqlx::query(
            "INSERT INTO feature_flag_wallets (environment, flag_name, wallet_address)
             VALUES (?, ?, ?)",
        )
        .bind(state.feature_flags.environment())
        .bind(&name)
        .bind(allowed_wallet)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_error("save feature flag allowlist wallet", e))?;
    }
    tx.commit()
        .await
        .map_err(|e| db_error("commit feature flag update", e))?;
    state
        .feature_flags
        .refresh(&state.db)
        .await
        .map_err(|e| db_error("refresh feature flag cache", e))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_admin_feature(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<StatusCode, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Operator, true).await?;
    let result = sqlx::query("DELETE FROM feature_flags WHERE environment = ? AND name = ?")
        .bind(state.feature_flags.environment())
        .bind(name)
        .execute(&state.db)
        .await
        .map_err(|e| db_error("delete feature flag", e))?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "feature flag not found",
        ));
    }
    state
        .feature_flags
        .refresh(&state.db)
        .await
        .map_err(|e| db_error("refresh feature flag cache", e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct Series {
    pub id: String,
    pub underlying: String,
    pub expires_at: String,
    pub active: bool,
}

pub async fn get_series(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
) -> Result<Json<Vec<Series>>, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Viewer, false).await?;
    let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT id, underlying, expires_at, active FROM admin_series ORDER BY expires_at",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("list admin series", e))?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, underlying, expires_at, active)| Series {
                id,
                underlying,
                expires_at,
                active,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct CreateSeriesRequest {
    pub underlying: String,
    pub expires_at: String,
    #[serde(default = "default_active")]
    pub active: bool,
}

fn default_active() -> bool {
    true
}

pub async fn post_series(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    AppJson(request): AppJson<CreateSeriesRequest>,
) -> Result<Json<Series>, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Operator, true).await?;
    if request.underlying.trim().is_empty() || request.expires_at.trim().is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "underlying and expires_at are required",
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO admin_series (id, underlying, expires_at, active) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&request.underlying)
    .bind(&request.expires_at)
    .bind(request.active)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("create admin series", e))?;
    Ok(Json(Series {
        id,
        underlying: request.underlying,
        expires_at: request.expires_at,
        active: request.active,
    }))
}

pub async fn delete_series(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Operator, true).await?;
    let deleted = sqlx::query("DELETE FROM admin_series WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| db_error("delete admin series", e))?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::new(StatusCode::NOT_FOUND, "series not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct CircuitBreaker {
    pub name: String,
    pub tripped: bool,
    pub reason: String,
    pub changed_by: String,
    pub updated_at: String,
}

pub async fn get_circuit_breakers(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
) -> Result<Json<Vec<CircuitBreaker>>, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Viewer, false).await?;
    let rows: Vec<(String, bool, String, String, String)> = sqlx::query_as(
        "SELECT name, tripped, reason, changed_by, updated_at FROM circuit_breakers ORDER BY name",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("list circuit breakers", e))?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(name, tripped, reason, changed_by, updated_at)| CircuitBreaker {
                    name,
                    tripped,
                    reason,
                    changed_by,
                    updated_at,
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct SetCircuitBreakerRequest {
    pub tripped: bool,
    #[serde(default)]
    pub reason: String,
}

pub async fn put_circuit_breaker(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    Path(name): Path<String>,
    AppJson(request): AppJson<SetCircuitBreakerRequest>,
) -> Result<StatusCode, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::RiskAdmin, true).await?;
    if name != "trading" {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "unknown circuit breaker",
        ));
    }
    sqlx::query(
        "INSERT INTO circuit_breakers (name, tripped, reason, changed_by)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(name) DO UPDATE SET tripped = excluded.tripped,
           reason = excluded.reason, changed_by = excluded.changed_by,
           updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(name)
    .bind(request.tripped)
    .bind(request.reason)
    .bind(wallet)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("update circuit breaker", e))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn trading_circuit_tripped(db: &sqlx::SqlitePool) -> Result<bool, sqlx::Error> {
    let tripped: Option<bool> =
        sqlx::query_scalar("SELECT tripped FROM circuit_breakers WHERE name = 'trading'")
            .fetch_optional(db)
            .await?;
    Ok(tripped.unwrap_or(false))
}

#[derive(Serialize)]
pub struct AdminUserInfo {
    pub wallet_address: String,
    pub roles: Vec<String>,
    pub account_created_at: Option<String>,
}

pub async fn get_user(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    Path(target_wallet): Path<String>,
) -> Result<Json<AdminUserInfo>, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::Viewer, false).await?;
    let roles: Vec<(String,)> =
        sqlx::query_as("SELECT role FROM admin_roles WHERE wallet_address = ? ORDER BY role")
            .bind(&target_wallet)
            .fetch_all(&state.db)
            .await
            .map_err(|e| db_error("look up target admin roles", e))?;
    let account_created_at: Option<String> =
        sqlx::query_scalar("SELECT created_at FROM accounts WHERE wallet_address = ?")
            .bind(&target_wallet)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| db_error("look up target account", e))?;
    if roles.is_empty() && account_created_at.is_none() {
        return Err(AppError::new(StatusCode::NOT_FOUND, "user not found"));
    }
    Ok(Json(AdminUserInfo {
        wallet_address: target_wallet,
        roles: roles.into_iter().map(|(role,)| role).collect(),
        account_created_at,
    }))
}

#[derive(Deserialize)]
pub struct GrantRoleRequest {
    pub role: AdminRole,
}

pub async fn post_user_role(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    Path(target_wallet): Path<String>,
    AppJson(request): AppJson<GrantRoleRequest>,
) -> Result<StatusCode, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::SuperAdmin, true).await?;
    crate::strkey::decode_stellar_public_key(&target_wallet).map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "wallet_address is not a valid Stellar G... address",
        )
    })?;
    sqlx::query(
        "INSERT INTO admin_roles (wallet_address, role, granted_by)
         VALUES (?, ?, ?) ON CONFLICT(wallet_address, role) DO NOTHING",
    )
    .bind(target_wallet)
    .bind(request.role.as_str())
    .bind(wallet)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("grant admin role", e))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_user_role(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    headers: HeaderMap,
    Path((target_wallet, role)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    require_role(&state, &wallet, &headers, AdminRole::SuperAdmin, true).await?;
    if !["viewer", "operator", "risk_admin", "super_admin"].contains(&role.as_str()) {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "unknown admin role"));
    }
    let deleted = sqlx::query("DELETE FROM admin_roles WHERE wallet_address = ? AND role = ?")
        .bind(target_wallet)
        .bind(role)
        .execute(&state.db)
        .await
        .map_err(|e| db_error("revoke admin role", e))?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::new(StatusCode::NOT_FOUND, "admin role not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}
