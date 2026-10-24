//! Explicit authorization invocation plans for the vault/oracle/settlement
//! contract boundary. Plans describe scope; they are not Soroban XDR auth
//! entries and must not be treated as signed authorization.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invocation {
    pub contract_id: String,
    pub function: String,
    /// Canonical argument XDR values, base64 encoded.
    pub args_xdr: Vec<String>,
    #[serde(default)]
    pub sub_invocations: Vec<Invocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressAuthorizationPlan {
    pub address: String,
    pub nonce: i64,
    pub signature_expiration_ledger: u32,
    pub root_invocation: Invocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationPlanError {
    InvalidAddress,
    InvalidContractId,
    InvalidFunction,
    InvalidArguments,
    Expired,
}

impl std::fmt::Display for AuthorizationPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAddress => write!(f, "authorization address must be a valid Stellar G address"),
            Self::InvalidContractId => write!(f, "invocation contract must be a valid Soroban C address"),
            Self::InvalidFunction => write!(f, "invocation function must not be empty"),
            Self::InvalidArguments => write!(f, "invocation arguments must be valid base64 XDR"),
            Self::Expired => write!(f, "authorization expiration ledger must be in the future"),
        }
    }
}

impl std::error::Error for AuthorizationPlanError {}

impl AddressAuthorizationPlan {
    /// Build an address-auth plan for the exact invocation tree returned by
    /// simulation. The caller must still assemble and sign real Soroban
    /// `SorobanAuthorizationEntry` XDR using the network SDK.
    pub fn from_simulated_invocation(
        address: &str,
        nonce: i64,
        signature_expiration_ledger: u32,
        current_ledger: u32,
        root_invocation: Invocation,
    ) -> Result<Self, AuthorizationPlanError> {
        if crate::strkey::decode_stellar_public_key(address).is_err() {
            return Err(AuthorizationPlanError::InvalidAddress);
        }
        if signature_expiration_ledger <= current_ledger {
            return Err(AuthorizationPlanError::Expired);
        }
        validate_invocation(&root_invocation)?;
        Ok(Self {
            address: address.to_string(),
            nonce,
            signature_expiration_ledger,
            root_invocation,
        })
    }
}

fn validate_invocation(invocation: &Invocation) -> Result<(), AuthorizationPlanError> {
    if crate::strkey::decode_contract_id(&invocation.contract_id).is_err() {
        return Err(AuthorizationPlanError::InvalidContractId);
    }
    if invocation.function.trim().is_empty() {
        return Err(AuthorizationPlanError::InvalidFunction);
    }
    if invocation
        .args_xdr
        .iter()
        .any(|arg| arg.is_empty() || data_encoding::BASE64.decode(arg.as_bytes()).is_err())
    {
        return Err(AuthorizationPlanError::InvalidArguments);
    }
    for child in &invocation.sub_invocations {
        validate_invocation(child)?;
    }
    Ok(())
}
