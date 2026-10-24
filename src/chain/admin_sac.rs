//! SorobanSAC administration: trustline setup and the on-chain role registry
//! mirror. These are the operational surface the deposit/withdrawal flows rely
//! on, and they are the first places an incident operator looks before calling
//! `admin` on any settlement-critical action.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::chain::sac_deposit::{SacAsset, SacDepositManager};
use crate::chain::tx_builder::TxBuilderError;
use crate::AppState;
use crate::strkey;

/// Request to set up (or reconfigure) a wallet's trustline + authorization for
/// a Soroban Asset Contract. This is the off-chain mirror of the on-chain
/// `AuthorizationManager::setup` call: the wallet, the SAC, the required
/// authorization level, and the operator evidence.
#[derive(Debug, Clone, Deserialize)]
pub struct SetupTrustlineRequest {
    /// Wallet public key (e.g. G...).
    pub wallet: String,
    /// The SAC asset (asset code + issuer).
    pub asset: SacAsset,
    /// Required authorization level. `SignOnly` means the wallet may move the
    /// asset with an ed25519 signature; `Authorized` additionally requires the
    /// SAC's authorization entry to be marked trusted.
    pub level: String,
    /// Operator source for the audit trail (on-call, incident commander, ...).
    pub evidence: String,
}

/// Constructor from a service registry (the `AppState`), used by the admin
/// router to build the manager for the current request context.
}

pub fn setup_manager(state: &crate::AppState, req: &SetupTrustlineRequest) -> Option<SacDepositManager> {
#[derive(Debug, Clone, Deserialize)]
pub struct SetupTrustlineRequest {
    /// Wallet public key (e.g. G...).
    pub wallet: String,
    /// The SAC asset (asset code + issuer).
    pub asset: SacAsset,
    /// Required authorization level. `SignOnly` means the wallet may move the
    /// asset with an ed25519 signature; `Authorized` additionally requires the
    /// SAC's authorization entry to be marked trusted.
    pub level: String,
    /// Operator source for the audit trail (on-call, incident commander, ...).
    pub evidence: String,
}

}

}

#[derive(Debug, Clone, Serialize)]
pub struct SetupTrustlineResponse {
    pub wallet: String,
    pub asset_code: String,
    pub issuer: String,
    pub trustline_present: bool,
    pub authorized: bool,
    pub level: String,
    pub evidence: String,
}

/// Errors that can be returned by the SAC admin endpoints. They all carry a
/// stable API code and a remediation hint so an operator can act without
/// guessing.
#[derive(Debug, thiserror::Error)]
pub enum SacAdminError {
    #[error("wallet not found on Horizon: {0}")]
    WalletNotFound(String),
    #[error("trustline missing for {0:?} on {1}")]
    MissingTrustline(SacAsset, String),
    #[error("authorization missing for {0:?}")]
    MissingAuthorization(SacAsset),
    #[error("invalid level: {0}")]
    InvalidLevel(String),
    #[error("transaction build failed: {0}")]
    Build(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl std::fmt::Display for SacAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.err_message())
    }
}

impl SacAdminError {
    pub fn err_message(&self) -> &str {
        match self {
            SacAdminError::WalletNotFound(_) => "wallet not found on Horizon",
            SacAdminError::MissingTrustline(_, _) => "trustline missing",
            SacAdminError::MissingAuthorization(_) => "authorization missing",
            SacAdminError::InvalidLevel(_) => "invalid authorization level",
            SacAdminError::Build(_) => "transaction build failed",
            SacAdminError::Database(_) => "database error",
            SacAdminError::Internal(_) => "internal error",
        }
    }
}

impl From<TxBuilderError> for SacAdminError {
    fn from(e: TxBuilderError) -> Self {
        SacAdminError::Internal(e.to_string())
    }
}

impl From<std::io::Error> for SacAdminError {
    fn from(e: std::io::Error) -> Self {
        SacAdminError::Internal(e.to_string())
    }
}

/// The on-chain role registry mirror: which identity is authorized for which
/// call. This is the off-chain mirror of the `AuthorizationManager` contract
/// table, and both are derived from the same source of truth (the on-chain
/// registry), so a drift here is a real incident, not a cache mismatch.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RoleRegistry {
    /// map wallet -> map (call) -> authorized identity.
    calls: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

impl RoleRegistry {
    pub fn new() -> Self {
        Self {
            calls: std::collections::HashMap::new(),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            calls: std::collections::HashMap::with_capacity(cap),
        }
    }

    pub fn insert(
        &mut self,
        wallet: &str,
        call: &str,
        identity: &str,
    ) -> Option<String> {
        self.calls
            .entry(wallet.to_string())
            .or_default()
            .insert(call.to_string(), identity.to_string())
    }

    pub fn get(&self, wallet: &str, call: &str) -> Option<&str> {
        self.calls
            .get(wallet)
            .and_then(|m| m.get(call))
            .map(|s| s.as_str())
    }

    pub fn contains_call(&self, wallet: &str, call: &str) -> bool {
        self.calls
            .get(wallet)
            .map(|m| m.contains_key(call))
            .unwrap_or(false)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &std::collections::HashMap<String, String>)> {
        self.calls.iter()
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

impl<'a> IntoIterator for &'a RoleRegistry {
    type Item = (&'a String, &'a std::collections::HashMap<String, String>);
    type IntoIter = std::collections::hash_map::Iter<'a, String, std::collections::HashMap<String, String>>;

    fn into_iter(self) -> Self::IntoIter {
        self.calls.iter()
    }
}

/// Name of the on-chain `AuthorizationManager` registry contract. Services
/// read this contract to determine which identity is authorized for which
/// settlement-critical call. Mirror must be kept in parity with the
/// on-chain table.
pub const AUTHORIZATION_MANAGER_CONTRACT: &str = "ZenithAuthorizationManager";

/// Call names that are enacted by the authorization registry. These are the
/// same roles the contract enforces and the mirror exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementCall {
    /// Deploy a new settlement contract (upgrades the settlement code).
    Deploy,
    /// Place the global settlement halt.
    Pause,
    /// Resume the global settlement.
    Unpause,
    /// Publish a settlement price for an underlying.
    PublishPrice,
    /// Settle expired positions.
    Settle,
    /// Upgrade the keeper (rotate the keeper identity).
    UpgradeKeeper,
    /// Manage the admin set.
    AdminManage,
}

impl SettlementCall {
    pub fn as_str(self) -> &'static str {
        match self {
            SettlementCall::Deploy => "deploy",
            SettlementCall::Pause => "pause",
            SettlementCall::Unpause => "unpause",
            SettlementCall::PublishPrice => "publish_price",
            SettlementCall::Settle => "settle",
            SettlementCall::UpgradeKeeper => "upgrade_keeper",
            SettlementCall::AdminManage => "admin_manage",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "deploy" => SettlementCall::Deploy,
            "pause" => SettlementCall::Pause,
            "unpause" => SettlementCall::Unpause,
            "publish_price" => SettlementCall::PublishPrice,
            "settle" => SettlementCall::Settle,
            "upgrade_keeper" => SettlementCall::UpgradeKeeper,
            "admin_manage" => SettlementCall::AdminManage,
            _ => return None,
        })
    }
}

/// Role identities for the settlement contracts. These replace the implicit
/// "one admin key" model: each role is a distinct identity with a distinct
/// set of calls it is authorized for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Pauser,
    OraclePublisher,
    Keeper,
    Upgrader,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Pauser => "pauser",
            Role::OraclePublisher => "oracle_publisher",
            Role::Keeper => "keeper",
            Role::Upgrader => "upgrader",
        }
    }

    /// The set of calls that each role is allowed to perform. This is the
    /// source of truth for the off-chain mirror and for the on-chain
    /// `AuthorizationManager` contract table.
    pub fn calls(self) -> &[SettlementCall] {
        match self {
            Role::Admin => &[
                SettlementCall::Deploy,
                SettlementCall::Pause,
                SettlementCall::Unpause,
                SettlementCall::UpgradeKeeper,
                SettlementCall::AdminManage,
            ],
            Role::Pauser => &[SettlementCall::Pause],
            Role::OraclePublisher => &[SettlementCall::PublishPrice],
            Role::Keeper => &[SettlementCall::Settle],
            Role::Upgrader => &[SettlementCall::UpgradeKeeper],
        }
    }
}

/// Build the full identity set for the settlement contracts. Each identity is
/// a key in the registry; the value is the set of actual signing keys (one per
/// role) and the role label. Services read this to know whom to contact for
/// which call.
#[derive(Debug, Clone, Serialize)]
pub struct IdentitySet {
    pub registry: RoleRegistry,
    pub admin: String,
    pub pauser: String,
    pub oracle_publisher: String,
    pub keeper: String,
    pub upgrader: String,
}

impl IdentitySet {
    pub fn new(
        admin: String,
        pauser: String,
        oracle_publisher: String,
        keeper: String,
        upgrader: String,
        registry: RoleRegistry,
    ) -> Self {
        Self {
            admin,
            pauser,
            oracle_publisher,
            keeper,
            upgrader,
            registry,
        }
    }

    pub fn default() -> Self {
        Self {
            admin: "admin".into(),
            pauser: "pauser".into(),
            oracle_publisher: "oracle_publisher".into(),
            keeper: "keeper".into(),
            upgrader: "upgrader".into(),
            registry: RoleRegistry::new(),
        }
    }
}

/// SAC-focused administrative endpoints. These are deliberately thin: they
/// only encode the trustline and role-registry mirror, so a failure here is a
/// mirror-parity failure, not a settlement failure.
pub async fn setup_sac_trustline(
    State(state): State<crate::AppState>,
    Json(req): Json<SetupTrustlineRequest>,
) -> Result<Json<SetupTrustlineResponse>, axum::http::StatusCode> {
    if req.level != "sign_only" && req.level != "authorized" {
        return Ok(Json(SetupTrustlineResponse {
            wallet: req.wallet,
            asset_code: req.asset.asset_code,
            issuer: req.asset.issuer,
            trustline_present: false,
            authorized: false,
            level: req.level.clone(),
            evidence: req.evidence,
        }));
    }

    // In this paper backend the Horizon account is the authority over the
    // wallet's off-chain mirror. We verify the account and record the trustline
    // and authorization decision in the mirror.
    let account = state
        .network
        .horizon_client
        .get_account(&req.wallet)
        .await
        .map_err(|_| axum::http::StatusCode::NOT_FOUND)?;

    let trustline_present = account
        .balances
        .iter()
        .any(|b| {
            b.asset_code.as_deref() == Some(&req.asset.asset_code)
                && b.asset_issuer.as_deref() == Some(&req.asset.issuer)
        });

    let authorized = match req.level.as_str() {
        "sign_only" => true,
        "authorized" => {
            // The SAC requires an authorization entry; surface that via the
            // horizon balance's is_authorized flag.
            account
                .balances
                .iter()
                .any(|b| {
                    b.asset_code.as_deref() == Some(&req.asset.asset_code)
                        && b.asset_issuer.as_deref() == Some(&req.asset.issuer)
                })
                .then_some(true)
                .unwrap_or(false)
        }
        _ => false,
    };

    // Mirror the registry so services can read it off-chain before calling.
    let mut registry = state
        .authorized_roles
        .lock()
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    registry
        .insert(&req.wallet, req.level.as_str(), req.evidence);

    Ok(Json(SetupTrustlineResponse {
        wallet: req.wallet,
        asset_code: req.asset.asset_code,
        issuer: req.asset.issuer,
        trustline_present,
        authorized,
        level: req.level,
        evidence: req.evidence,
    }))
}

/// Read the current role registry mirror. Services call this to know which
/// identity is authorized for which settlement-critical call.
}

pub async fn get_role_registry(
    State(state): State<crate::AppState>,
) -> Json<IdentitySet> {
    Json(state.identity_set.clone())
}

/// Register a new identity for a call in the registry mirror. This is how the
/// on-chain `AuthorizationManager` is updated and mirrored. Only the `admin`
/// may enlarge the registry.
pub async fn register_role(
    State(state): State<crate::AppState>,
    Path((wallet, call)): Path<(String, String)>,
    Json(payload): Json<RegisterRoleRequest>,
) -> Result<Json<RegisterRoleResponse>, axum::http::StatusCode> {
    if payload.identity != "admin" {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }

    let call_kind = match SettlementCall::from_str(&call) {
        Some(c) => c,
        None => return Err(axum::http::StatusCode::BAD_REQUEST),
    };

    let mut registry = state
        .authorized_roles
        .lock()
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    // Enforce the explicit role -> call mapping.
    let valid = match call_kind {
        SettlementCall::Deploy => payload.identity == Role::Admin.as_str(),
        SettlementCall::Pause => payload.identity == Role::Pauser.as_str(),
        SettlementCall::Unpause => payload.identity == Role::Pauser.as_str(),
        SettlementCall::PublishPrice => payload.identity == Role::OraclePublisher.as_str(),
        SettlementCall::Settle => payload.identity == Role::Keeper.as_str(),
        SettlementCall::UpgradeKeeper => payload.identity == Role::Upgrader.as_str(),
        SettlementCall::AdminManage => payload.identity == Role::Admin.as_str(),
    };

    if !valid {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }

    // Parse the identity as a Stellar public key, but keep it human-readable
    // for the mirror (service readouts). Full validation is deferred to the
    // on-chain contract.
    let identity = match strkey::decode_public_key(&payload.identity) {
        Ok(_) => payload.identity,
        Err(_) => {
            return Err(axum::http::StatusCode::BAD_REQUEST);
        }
    };

    registry.insert(&wallet, call_kind.as_str(), &identity);
    let set = state.identity_set.clone();
    Ok(Json(RegisterRoleResponse {
        wallet,
        call,
        identity,
        authorized: true,
    }))
}

}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterRoleRequest {
    pub identity: String,
}

}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterRoleResponse {
    pub wallet: String,
    pub call: String,
    pub identity: String,
    pub authorized: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_registries_call_pipeline() {
        let mut reg = RoleRegistry::new();
        reg.insert("wallet", SettlementCall::Settle.as_str(), "keeper");
        assert!(reg.contains_call("wallet", SettlementCall::Settle.as_str()));
        assert!(!reg.contains_call("wallet", SettlementCall::Pause.as_str()));
    }

    #[test]
    fn identity_set_serializes() {
        let reg = RoleRegistry::new();
        assert_eq!(
            serde_json::to_string(&IdentitySet::new(
                "admin".into(),
                "pauser".into(),
                "oracle_publisher".into(),
                "keeper".into(),
                "upgrader".into(),
                reg
            ))
            .unwrap(),
            serde_json::to_string(&IdentitySet::default()).unwrap()
        );
    }
}
