//! Settlement role admin endpoints. These are the off-chain surface that
//! updates the explicit on-chain role registry and mirrors it so services can
//! read which identity is authorized for which call.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::chain::admin_sac::RoleRegistry;
use crate::chain::roles::{Call, Role};
use crate::AppState;
use crate::chain::roles::check_call;

#[derive(Debug, Clone, Deserialize)]
pub struct SetRoleRequest {
    pub role: String,
    pub call: String,
    pub identity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetRoleResponse {
    pub role: String,
    pub call: String,
    pub identity: String,
    pub authorized: bool,
}

pub async fn set_role(
    State(state): State<crate::AppState>,
    Path((role, call)): Path<(String, String)>,
    Json(req): Json<SetRoleRequest>,
) -> Result<Json<SetRoleResponse>, axum::http::StatusCode> {
    let role = match Role::from_str(&role) {
        Some(r) => r,
        None => return Err(axum::http::StatusCode::BAD_REQUEST),
    };
    let call = match Call::from_str(&call) {
        Some(c) => c,
        None => return Err(axum::http::StatusCode::BAD_REQUEST),
    };

    if req.identity != "admin" {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    if !crate::chain::roles::check_call(&role, &call).is_ok() {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }

    // The explicit role → call mapping, replicated here so the mirror and
    // the on-chain enforcement cannot silently drift apart.
    let mapping_ok = match (role, call) {
        (Role::Admin, Call::Deploy) => true,
        (Role::Pauser, Call::Pause) => true,
        (Role::Pauser, Call::Unpause) => true,
        (Role::OraclePublisher, Call::PublishPrice) => true,
        (Role::Keeper, Call::Settle) => true,
        (Role::Upgrader, Call::Upgrade) => true,
        _ => false,
    };

    if !mapping_ok {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }

    let mut registry = state
        .authorized_roles
        .lock()
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    registry.insert(role.as_str(), call.as_str(), &req.identity);

    Ok(Json(SetRoleResponse {
        role: req.role,
        call: req.call,
        identity: req.identity,
        authorized: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_registry_pipeline() {
        let reg = RoleRegistry::new();
        assert!(reg.is_empty());
    }
}
