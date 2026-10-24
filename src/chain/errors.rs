//! Decoded error mapping for the transaction simulation preflight API.
//!
//! Soroban simulation failures surface as opaque host error codes. This module
//! translates them into stable API error codes with human-readable messages and
//! remediation hints so integrators can act before requesting a signature.
//!
//! Two classes of errors are handled:
//! * Zenith contract errors declared via `#[contracterror]` (mapped from the
//!   contract spec / error codes).
//! * Soroban host errors (budget exceeded, archived storage entries, auth
//!   failures, etc.).

use serde::{Deserialize, Serialize};

/// Stable API error code returned to clients. These are intentionally decoupled
/// from the on-chain numeric codes so the API surface stays stable even if the
/// contract renumbers its errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApiErrorCode {
    /// Insufficient collateral to satisfy the requested action.
    InsufficientCollateral,
    /// The referenced series has expired.
    SeriesExpired,
    /// The referenced series does not exist.
    SeriesNotFound,
    /// A required trustline is missing.
    MissingTrustline,
    /// The caller is not authorized for this action.
    Unauthorized,
    /// The transaction would exceed the network CPU instruction budget.
    BudgetExceeded,
    /// A storage entry required by the transaction has been archived.
    StorageArchived,
    /// Authentication requirements were not satisfied.
    AuthFailure,
    /// A Soroban JSON-RPC request used an unsupported method.
    RpcMethodUnsupported,
    /// A Soroban RPC endpoint or transport failed.
    RpcUnavailable,
    /// The action or XDR could not be parsed.
    InvalidRequest,
    /// An error that could not be mapped to a more specific code.
    Unknown,
}

/// A decoded error with a stable code, message, and remediation hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodedError {
    /// Stable API error code.
    pub code: ApiErrorCode,
    /// HTTP status appropriate for an API response carrying this failure.
    pub http_status: u16,
    /// Human-readable message describing the failure.
    pub message: String,
    /// Actionable remediation hint for the integrator.
    pub remediation: String,
    /// The raw on-chain error code, when one was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_code: Option<u32>,
    /// Raw JSON-RPC error code, when the failure came from a JSON-RPC reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_rpc_code: Option<i64>,
}

impl DecodedError {
    /// Build a decoded error.
    pub fn new(
        code: ApiErrorCode,
        message: &str,
        remediation: &str,
        raw_code: Option<u32>,
    ) -> Self {
        Self {
            code,
            http_status: match code {
                ApiErrorCode::InvalidRequest => 400,
                ApiErrorCode::Unauthorized | ApiErrorCode::AuthFailure => 403,
                ApiErrorCode::SeriesNotFound => 404,
                ApiErrorCode::SeriesExpired => 409,
                ApiErrorCode::RpcMethodUnsupported | ApiErrorCode::RpcUnavailable => 503,
                ApiErrorCode::Unknown => 500,
                _ => 422,
            },
            message: message.to_string(),
            remediation: remediation.to_string(),
            raw_code,
            raw_rpc_code: None,
        }
    }
}

/// Translate a JSON-RPC 2.0 error code into a stable API error. The upstream
/// message is retained as context, but clients should branch on `code`.
pub fn map_rpc_error_code(raw_code: i64, raw_message: &str) -> DecodedError {
    let (api_code, message, remediation) = match raw_code {
        -32700 | -32600 | -32602 => (
            ApiErrorCode::InvalidRequest,
            "The Soroban RPC request was invalid",
            "Check the request method and parameters before retrying.",
        ),
        -32601 => (
            ApiErrorCode::RpcMethodUnsupported,
            "The Soroban RPC endpoint does not support this method",
            "Use an endpoint that supports the requested Soroban RPC method.",
        ),
        -32603 => (
            ApiErrorCode::RpcUnavailable,
            "The Soroban RPC endpoint could not complete the request",
            "Retry against a healthy endpoint; contact the operator if this persists.",
        ),
        _ => (
            ApiErrorCode::Unknown,
            "The Soroban RPC request failed",
            "Inspect the raw RPC code and message, then consult the endpoint documentation.",
        ),
    };
    let mut decoded = DecodedError::new(
        api_code,
        message,
        remediation,
        None,
    );
    decoded.raw_rpc_code = Some(raw_code);
    if !raw_message.is_empty() {
        decoded.message = format!("{message}: {raw_message}");
    }
    decoded
}

/// A single entry in the Zenith `#[contracterror]` mapping table.
///
/// The table is generated from the contract spec where possible; the numeric
/// codes below mirror the `#[contracterror]` discriminants declared by the
/// Zenith contract.
pub struct ContractErrorMapping {
    /// Numeric `#[contracterror]` code emitted by the contract.
    pub contract_code: u32,
    /// Stable API error code.
    pub api_code: ApiErrorCode,
    /// Human-readable message.
    pub message: &'static str,
    /// Remediation hint.
    pub remediation: &'static str,
}

/// Mapping table from Zenith `#[contracterror]` codes to stable API errors.
///
/// Keep this in sync with the contract's `#[contracterror]` enum. When the
/// contract spec is available at build time this table can be generated from
/// it; the static table below is the source of truth for the API surface.
pub const ZENITH_CONTRACT_ERRORS: &[ContractErrorMapping] = &[
    ContractErrorMapping {
        contract_code: 1,
        api_code: ApiErrorCode::InsufficientCollateral,
        message: "Insufficient collateral for the requested action",
        remediation: "Deposit additional collateral or reduce the requested amount before retrying.",
    },
    ContractErrorMapping {
        contract_code: 2,
        api_code: ApiErrorCode::SeriesExpired,
        message: "The referenced series has expired",
        remediation: "Select an active series or roll the position into a new series.",
    },
    ContractErrorMapping {
        contract_code: 3,
        api_code: ApiErrorCode::SeriesNotFound,
        message: "The referenced series does not exist",
        remediation: "Verify the series identifier and ensure it has been initialized.",
    },
    ContractErrorMapping {
        contract_code: 4,
        api_code: ApiErrorCode::MissingTrustline,
        message: "A required trustline is missing",
        remediation: "Establish the required trustline for the asset before retrying.",
    },
    ContractErrorMapping {
        contract_code: 5,
        api_code: ApiErrorCode::Unauthorized,
        message: "The caller is not authorized for this action",
        remediation: "Ensure the transaction is signed by the required account and satisfies auth requirements.",
    },
];

/// Look up a Zenith contract error by its numeric `#[contracterror]` code.
pub fn map_contract_error(contract_code: u32) -> DecodedError {
    match ZENITH_CONTRACT_ERRORS
        .iter()
        .find(|m| m.contract_code == contract_code)
    {
        Some(m) => DecodedError::new(m.api_code, m.message, m.remediation, Some(contract_code)),
        None => DecodedError::new(
            ApiErrorCode::Unknown,
            "Unrecognized contract error",
            "Inspect the raw contract error code and consult the contract spec.",
            Some(contract_code),
        ),
    }
}

/// Recognised Soroban host error classes relevant to the preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorClass {
    /// The transaction would exceed the network CPU instruction budget.
    BudgetExceeded,
    /// A storage entry required by the transaction has been archived.
    StorageArchived,
    /// Authentication requirements were not satisfied.
    AuthFailure,
}

/// Map a recognised host error class to a decoded error.
pub fn map_host_error(class: HostErrorClass) -> DecodedError {
    match class {
        HostErrorClass::BudgetExceeded => DecodedError::new(
            ApiErrorCode::BudgetExceeded,
            "The transaction would exceed the network CPU instruction budget",
            "Reduce the number of operations or split the action into smaller transactions.",
            None,
        ),
        HostErrorClass::StorageArchived => DecodedError::new(
            ApiErrorCode::StorageArchived,
            "A storage entry required by the transaction has been archived",
            "Restore the archived entries using the provided restore preamble, then retry.",
            None,
        ),
        HostErrorClass::AuthFailure => DecodedError::new(
            ApiErrorCode::AuthFailure,
            "Authentication requirements were not satisfied",
            "Provide the required signatures and ensure the auth entries match the invocation.",
            None,
        ),
    }
}

/// Map a host error's numeric payload when its XDR error class was not
/// available. The integer alone is not enough to safely infer a Soroban host
/// error category, so it is preserved as unknown instead of guessed.
pub fn map_host_error_code(raw_code: u32) -> DecodedError {
    DecodedError::new(
        ApiErrorCode::Unknown,
        "Unclassified Soroban host error",
        "Inspect the host error class and numeric code in the Soroban XDR response.",
        Some(raw_code),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_contract_error() {
        let decoded = map_contract_error(1);
        assert_eq!(decoded.code, ApiErrorCode::InsufficientCollateral);
        assert_eq!(decoded.raw_code, Some(1));
        assert!(!decoded.remediation.is_empty());
    }

    #[test]
    fn maps_unknown_contract_error() {
        let decoded = map_contract_error(9999);
        assert_eq!(decoded.code, ApiErrorCode::Unknown);
        assert_eq!(decoded.raw_code, Some(9999));
    }

    #[test]
    fn maps_typed_host_error_classes() {
        assert_eq!(
            map_host_error(HostErrorClass::BudgetExceeded).code,
            ApiErrorCode::BudgetExceeded
        );
        assert_eq!(
            map_host_error(HostErrorClass::StorageArchived).code,
            ApiErrorCode::StorageArchived
        );
        assert_eq!(
            map_host_error(HostErrorClass::AuthFailure).code,
            ApiErrorCode::AuthFailure
        );
    }

    #[test]
    fn maps_unknown_host_error() {
        let decoded = map_host_error_code(0xDEAD_BEEF);
        assert_eq!(decoded.code, ApiErrorCode::Unknown);
        assert_eq!(decoded.raw_code, Some(0xDEAD_BEEF));
    }

    #[test]
    fn maps_json_rpc_errors_to_stable_codes() {
        let invalid = map_rpc_error_code(-32602, "missing accountId");
        assert_eq!(invalid.code, ApiErrorCode::InvalidRequest);
        assert_eq!(invalid.raw_rpc_code, Some(-32602));
        assert!(invalid.message.contains("missing accountId"));

        let unsupported = map_rpc_error_code(-32601, "getLedgerEntries");
        assert_eq!(unsupported.code, ApiErrorCode::RpcMethodUnsupported);
    }
}
