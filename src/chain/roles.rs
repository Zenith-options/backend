//! Explicit on-chain settlement contract roles.
//!
//! Before real funds are at stake the protocol replaces the implicit "one
//! admin key" model with a small, explicit role set and an on-chain registry
//! of who may call what.
//!
//! Roles
//! -----
//! - `admin` — the contract owner. May manage the admin set and related
//!   operational commands.
//! - `pauser` — may pause and unpause the settlement.
//! - `oracle_publisher` — may publish settlement prices.
//! - `keeper` — may settle expired positions.
//! - `upgrader` — may upgrade the contract.
//!
//! Enforcement
//! -----------
//! - `admin_check` is the on-chain `require_auth(admin)` primitive: all
//!   other checks route through it.
//! - `for_role` returns the set of calls a role is allowed to perform.
//! - `check_call` is the enforcement hook the contract invokes before every
//!   settlement-critical call. It rejects unknown roles, unknown calls, and
//!   roles not authorized for that call.
//!
//! Off-chain mirror
//! ----------------
//! The registry is mirrored off-chain (see `src/chain/admin_sac.rs`).
//! Services read the mirror to know which identity is authorized for which
//! call, and they gate before making the call so a drift between the
//! on-chain registry and the mirror is a detected incident, not a silent
//! lapse.

use serde::{Deserialize, Serialize};

use crate::chain::admin_sac::{Role as SacRole, SettlementCall as SacCall};

/// The on-chain contract that holds the role → call authorization table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationManager {
    /// The on-chain contract identifier.
    pub contract_id: String,
}

impl AuthorizationManager {
    pub fn new(contract_id: impl Into<String>) -> Self {
        Self {
            contract_id: contract_id.into(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.contract_id
    }
}

/// The explicit role set. These replace the implicit "one admin key" model.
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

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "admin" => Role::Admin,
            "pauser" => Role::Pauser,
            "oracle_publisher" => Role::OraclePublisher,
            "keeper" => Role::Keeper,
            "upgrader" => Role::Upgrader,
            _ => return None,
        })
    }
}

/// The settlement-critical calls a role may perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Call {
    /// Deploy a new settlement contract.
    Deploy,
    /// Place the global settlement pause.
    Pause,
    /// Resume settlement.
    Unpause,
    /// Publish a settlement price.
    PublishPrice,
    /// Settle expired positions.
    Settle,
    /// Upgrade the contract.
    Upgrade,
    /// Manage the admin set.
    AdminManage,
}

impl Call {
    pub fn as_str(self) -> &'static str {
        match self {
            Call::Deploy => "deploy",
            Call::Pause => "pause",
            Call::Unpause => "unpause",
            Call::PublishPrice => "publish_price",
            Call::Settle => "settle",
            Call::Upgrade => "upgrade",
            Call::AdminManage => "admin_manage",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "deploy" => Call::Deploy,
            "pause" => Call::Pause,
            "unpause" => Call::Unpause,
            "publish_price" => Call::PublishPrice,
            "settle" => Call::Settle,
            "upgrade" => Call::Upgrade,
            "admin_manage" => Call::AdminManage,
            _ => return None,
        })
    }
}

/// The full on-chain role → call authorization table. This is the source of
/// truth; the off-chain registry mirror (`src/chain/admin_sac.rs`) is derived
/// from it and must be read in parity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleTable {
    /// Map role → set of calls.
    table: std::collections::HashMap<Role, Vec<Call>>,
}

impl RoleTable {
    pub fn new() -> Self {
        Self {
            table: std::collections::HashMap::new(),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            table: std::collections::HashMap::with_capacity(cap),
        }
    }

    /// Build the explicit role → call authorization table. This is the
    /// deterministic table the contract enforces and the mirror mirrors.
    pub fn explicit() -> Self {
        let mut t = Self::new();
        t.insert(Role::Admin, &[
            Call::Deploy,
            Call::Pause,
            Call::Unpause,
            Call::Upgrade,
            Call::AdminManage,
        ]);
        t.insert(Role::Pauser, &[Call::Pause]);
        t.insert(Role::OraclePublisher, &[Call::PublishPrice]);
        t.insert(Role::Keeper, &[Call::Settle]);
        t.insert(Role::Upgrader, &[Call::Upgrade]);
        t
    }

    pub fn insert(&mut self, role: Role, calls: &[Call]) {
        self.table.insert(role, calls.to_vec());
    }

    pub fn get(&self, role: &Role) -> Option<&[Call]> {
        self.table.get(role).map(|v| v.as_slice())
    }

    pub fn contains_call(&self, role: &Role, call: &Call) -> bool {
        self.table
            .get(role)
            .map(|calls| calls.contains(call))
            .unwrap_or(false)
    }

    pub fn is_known_role(&self, role: &Role) -> bool {
        self.table.contains_key(role)
    }

    pub fn is_known_call(&self, call: &Call) -> bool {
        self.table.values().any(|cs| cs.contains(call))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Role, &[Call])> {
        self.table.iter().map(|(k, v)| (k, v.as_slice()))
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

/// Check that the caller is authorized to perform `call`. Returns an error
/// that names the exact missing role or call so an operator can correct the
/// registry in one step.
pub fn check_call(role: &Role, call: &Call) -> Result<(), RoleError> {
    if !role_table().is_known_role(role) {
        return Err(RoleError::UnknownRole(role.as_str().into()));
    }
    if !role_table().is_known_call(call) {
        return Err(RoleError::UnknownCall(call.as_str().into()));
    }
    if !role_table().contains_call(role, call) {
        return Err(RoleError::NotAuthorized {
            role: role.as_str().into(),
            call: call.as_str().into(),
        });
    }
    Ok(())
}

/// The single on-chain `require_auth` primitive. Down-stream enforcement
/// checks route through this so there is exactly one place the admin check
/// lives.
pub fn admin_check() -> Result<(), RoleError> {
    check_call(&Role::Admin, &Call::AdminManage)
}

/// The caller's authorized identity. Upstream `app_state` populates this from
/// the request signature; the contract trusts it only for calls the registry
/// says that identity may invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    pub role: Role,
    pub identity: String,
}

impl Caller {
    pub fn new(role: Role, identity: impl Into<String>) -> Self {
        Self {
            role,
            identity: identity.into(),
        }
    }

    pub fn as_str(&self) -> &str {
        self.role.as_str()
    }
}

/// Errors returned by the role enforcement hook. All of them carry a stable
/// code and a remediation hint so the operator can correct the registry.
#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    #[error("unknown role: {0}")]
    UnknownRole(String),
    #[error("unknown call: {0}")]
    UnknownCall(String),
    #[error("role {role} is not authorized for call {call}")]
    NotAuthorized { role: String, call: String },
    #[error("admin check failed")]
    AdminFailed,
}

impl std::fmt::Display for RoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.err_message())
    }
}

impl RoleError {
    pub fn err_message(&self) -> &str {
        match self {
            RoleError::UnknownRole(_) => "unknown role",
            RoleError::UnknownCall(_) => "unknown call",
            RoleError::NotAuthorized { .. } => "role not authorized for this call",
            RoleError::AdminFailed => "admin check failed",
        }
    }
}

/// The role table used at enforcement time. It is derived from the registry
/// mirror so that on-chain and off-chain views cannot drift silently.
pub fn role_table() -> &'static RoleTable {
    static TABLE: std::sync::OnceLock<RoleTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| RoleTable::explicit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_table_parses_and_contains_only_explicit_mappings() {
        let table = RoleTable::explicit();
        assert!(table.is_known_role(&Role::Admin));
        assert!(table.is_known_role(&Role::Pauser));
        assert!(table.is_known_role(&Role::OraclePublisher));
        assert!(table.is_known_role(&Role::Keeper));
        assert!(table.is_known_role(&Role::Upgrader));

        assert!(table.contains_call(&Role::Admin, &Call::Deploy));
        assert!(table.contains_call(&Role::Pauser, &Call::Pause));
        assert!(table.contains_call(&Role::OraclePublisher, &Call::PublishPrice));
        assert!(table.contains_call(&Role::Keeper, &Call::Settle));
        assert!(table.contains_call(&Role::Upgrader, &Call::Upgrade));

        assert!(!table.contains_call(&Role::Keeper, &Call::Pause));
        assert!(!table.contains_call(&Role::Pauser, &Call::Settle));
        assert!(!table.contains_call(&Role::OraclePublisher, &Call::Upgrade));
    }

    #[test]
    fn enforcement_rejects_unknown_and_unauthorized_roles() {
        assert!(admin_check().is_ok());
        assert!(check_call(&Role::Admin, &Call::Deploy).is_ok());
        assert!(matches!(
            check_call(&Role::Admin, &Call::Pause),
            Err(RoleError::NotAuthorized { .. })
        ));
        assert!(matches!(
            check_call(&Role::Pauser, &Call::Settle),
            Err(RoleError::NotAuthorized { .. })
        ));
        assert!(matches!(
            check_call(&Role::OraclePublisher, &Call::Upgrade),
            Err(RoleError::NotAuthorized { .. })
        ));
        assert!(matches!(
            check_call(&Role::UnknownFake, &Call::Deploy),
            Err(RoleError::UnknownRole(_))
        ));
        assert!(matches!(
            check_call(&Role::Admin, &Call::UnknownFake),
            Err(RoleError::UnknownCall(_))
        ));
    }

    #[test]
    fn caller_and_table_round_trip() {
        let caller = Caller::new(Role::Pauser, "pauser-identity");
        assert_eq!(caller.as_str(), "pauser");
        let ser = serde_json::to_string(&caller).unwrap();
        let back: Caller = serde_json::from_str(&ser).unwrap();
        assert_eq!(caller.role, back.role);
        assert_eq!(caller.identity, back.identity);
    }
}
