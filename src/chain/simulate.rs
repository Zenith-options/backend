//! Transaction simulation: footprint extraction, resource fees and typed
//! errors.
//!
//! A Soroban transaction is only valid if it declares
//!
//! * the **ledger footprint** it reads and writes (the simulation is the only
//!   way to learn it — the host resolves the contract state), and
//! * the **resource fee** that pays for the CPU/memory the host will use.
//!
//! `simulateTransaction` returns both, so the envelope is built from the
//! simulation instead of from hard-coded values. When the simulation cannot be
//! used — because the footprint was invalidated by a ledger advance, because
//! the contract failed, or because entries have to be restored first — the
//! failure is turned into a typed error the caller can act on.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use super::errors::{
    map_contract_error, map_host_error, ApiErrorCode, DecodedError, HostErrorClass,
};
use super::rpc::{RpcError, SorobanRpc};
use super::types::{SimulateFootprint, SimulateResources, SimulateTransactionResponse};

/// Number of ledgers a simulated footprint stays valid for.
///
/// Soroban footprints are only valid for a short window: once the chain has
/// advanced past it, the declared entries may no longer describe the state the
/// transaction expects and the transaction has to be simulated again.
pub const DEFAULT_FOOTPRINT_TTL_LEDGERS: u32 = 2;

/// Default price sheet used to turn declared resources into a resource fee.
///
/// The values are in stroops per unit. They are deliberately conservative: the
/// fee the node reports as `minResourceFee` is always used as a lower bound,
/// so a mis-calibrated price sheet can only ever over-collect, never
/// under-collect.
pub const DEFAULT_FEE_PER_INSTRUCTION: i64 = 1;
pub const DEFAULT_FEE_PER_READ_BYTE: i64 = 1;
pub const DEFAULT_FEE_PER_WRITE_BYTE: i64 = 1;
pub const DEFAULT_FEE_PER_EVENT: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SorobanAuthorizationEntry {
    pub credentials_type: String, // "address"
    pub address: String,          // C... or G... address
    pub nonce: i64,
    pub signature_expiration_ledger: u32,
    pub signature_args: Vec<String>, // passkey webauthn signature or ed25519 signature
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulateAuthResult {
    pub success: bool,
    pub auth_executed: bool,
    pub error: Option<String>,
}

pub struct TransactionSimulator;

impl TransactionSimulator {
    pub async fn simulate_check_auth(
        contract_id: &str,
        auth_entry: &SorobanAuthorizationEntry,
        current_ledger: u32,
    ) -> SimulateAuthResult {
        // 1. Verify signature has not expired
        if auth_entry.signature_expiration_ledger <= current_ledger {
            return SimulateAuthResult {
                success: false,
                auth_executed: false,
                error: Some(format!(
                    "Signature expired at ledger {} (current: {})",
                    auth_entry.signature_expiration_ledger, current_ledger
                )),
            };
        }

        // 2. Verify target contract
        if auth_entry.address != contract_id {
            return SimulateAuthResult {
                success: false,
                auth_executed: false,
                error: Some("Authorization entry address does not match contract ID".into()),
            };
        }

        // 3. Check signature payload exists
        if auth_entry.signature_args.is_empty() {
            return SimulateAuthResult {
                success: false,
                auth_executed: false,
                error: Some("No signature arguments provided for __check_auth".into()),
            };
        }

        // Simulated successful host __check_auth execution
        SimulateAuthResult {
            success: true,
            auth_executed: true,
            error: None,
        }
    }
}

/// The price sheet that converts declared resources into a resource fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePricing {
    /// Fee per CPU instruction, in stroops.
    pub fee_per_instruction: i64,
    /// Fee per byte read from a ledger entry, in stroops.
    pub fee_per_read_byte: i64,
    /// Fee per byte written to a ledger entry, in stroops.
    pub fee_per_write_byte: i64,
    /// Fee per emitted contract event, in stroops.
    pub fee_per_event: i64,
}

impl Default for ResourcePricing {
    fn default() -> Self {
        Self {
            fee_per_instruction: DEFAULT_FEE_PER_INSTRUCTION,
            fee_per_read_byte: DEFAULT_FEE_PER_READ_BYTE,
            fee_per_write_byte: DEFAULT_FEE_PER_WRITE_BYTE,
            fee_per_event: DEFAULT_FEE_PER_EVENT,
        }
    }
}

/// The result of a successful simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedTransaction {
    /// The transaction data the node produced (base64 envelope with the
    /// footprint attached).
    pub transaction_data: String,
    /// The minimum resource fee the node charges for this transaction.
    pub min_resource_fee: i64,
    /// The declared resources and the ledger footprint.
    pub resources: SimulateResources,
    /// The ledger the simulation was performed at.
    pub simulated_ledger: u32,
    /// The entries that have to be restored before the transaction can be
    /// submitted, when the node reports archived entries.
    pub restore_footprint: Option<SimulateFootprint>,
}

impl SimulatedTransaction {
    /// The ledger footprint of the simulated transaction.
    pub fn footprint(&self) -> Option<&SimulateFootprint> {
        self.resources.footprint.as_ref()
    }

    /// The ledger the footprint is valid for.
    pub fn footprint_valid_until(&self, ttl_ledgers: u32) -> u32 {
        footprint_valid_until(self.simulated_ledger, ttl_ledgers)
    }

    /// Whether the footprint is still valid at `current_ledger`.
    pub fn footprint_is_valid(&self, current_ledger: u32, ttl_ledgers: u32) -> bool {
        current_ledger <= self.footprint_valid_until(ttl_ledgers)
    }
}

/// A failure of a simulation, translated into something the caller can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimulationError {
    /// The contract rejected the transaction.
    ContractError {
        /// The contract error code, when the host reported one.
        code: Option<u32>,
        /// The raw error reported by the host.
        message: String,
    },
    /// The transaction declares more resources than the network allows.
    ResourceLimitExceeded { detail: String },
    /// Some of the entries the transaction needs have been archived.
    ArchivedEntries {
        /// The archived entries, as reported by the node.
        entries: Vec<String>,
        /// The `restoreFootprint` the node handed back, if any.
        restore_footprint: Option<SimulateFootprint>,
    },
    /// The node did not report a ledger footprint, so no compliant envelope
    /// can be built.
    FootprintUnavailable { detail: String },
    /// The chain advanced while the simulation was in flight, so the footprint
    /// has to be recomputed.
    LedgerAdvanced {
        simulated_ledger: u32,
        current_ledger: u32,
    },
    /// The node could not be reached or answered with an error.
    Rpc(RpcError),
    /// The response could not be understood.
    MalformedResponse { detail: String },
}

impl SimulationError {
    /// A stable machine-readable code for the failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ContractError { .. } => "CONTRACT_ERROR",
            Self::ResourceLimitExceeded { .. } => "RESOURCE_LIMIT_EXCEEDED",
            Self::ArchivedEntries { .. } => "ARCHIVED_ENTRIES",
            Self::FootprintUnavailable { .. } => "FOOTPRINT_UNAVAILABLE",
            Self::LedgerAdvanced { .. } => "LEDGER_ADVANCED",
            Self::Rpc(_) => "RPC_ERROR",
            Self::MalformedResponse { .. } => "MALFORMED_RESPONSE",
        }
    }

    /// The error as the API reports it: a stable code, a message and a
    /// remediation hint the integrator can act on.
    pub fn decoded(&self) -> DecodedError {
        match self {
            Self::ContractError {
                code: Some(code), ..
            } => map_contract_error(*code),
            Self::ContractError { message, .. } => {
                DecodedError::new(ApiErrorCode::Unknown, message, &self.remediation(), None)
            }
            Self::ResourceLimitExceeded { .. } => map_host_error(HostErrorClass::BudgetExceeded),
            Self::ArchivedEntries { .. } => map_host_error(HostErrorClass::StorageArchived),
            Self::FootprintUnavailable { .. } | Self::MalformedResponse { .. } => {
                DecodedError::new(
                    ApiErrorCode::InvalidRequest,
                    &self.to_string(),
                    &self.remediation(),
                    None,
                )
            }
            Self::LedgerAdvanced { .. } => DecodedError::new(
                ApiErrorCode::InvalidRequest,
                &self.to_string(),
                &self.remediation(),
                None,
            ),
            Self::Rpc(err) => DecodedError::new(
                ApiErrorCode::Unknown,
                &err.to_string(),
                &self.remediation(),
                None,
            ),
        }
    }

    /// What the caller should do about it.
    pub fn remediation(&self) -> String {
        match self {
            Self::ContractError { code, .. } => match code {
                Some(code) => {
                    format!("Resolve contract error #{code} before submitting the transaction.")
                }
                None => "Inspect the contract error and fix the invocation.".to_string(),
            },
            Self::ResourceLimitExceeded { .. } => {
                "Reduce the work the transaction does so it fits in the resource limits.".to_string()
            }
            Self::ArchivedEntries { .. } => {
                "Restore the archived entries with the returned restoreFootprint, then retry."
                    .to_string()
            }
            Self::FootprintUnavailable { .. } => {
                "Use a node that reports the ledger footprint, or supply a pre-computed footprint."
                    .to_string()
            }
            Self::LedgerAdvanced {
                simulated_ledger,
                current_ledger,
            } => format!(
                "Re-simulate the transaction: the chain advanced from ledger {simulated_ledger} to {current_ledger}."
            ),
            Self::Rpc(err) => format!("Retry once the node is reachable ({err})."),
            Self::MalformedResponse { .. } => {
                "Retry the simulation; the node returned an unreadable response.".to_string()
            }
        }
    }
}

impl std::fmt::Display for SimulationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContractError { code, message } => match code {
                Some(code) => write!(f, "contract error #{code}: {message}"),
                None => write!(f, "contract error: {message}"),
            },
            Self::ResourceLimitExceeded { detail } => {
                write!(f, "resource limit exceeded: {detail}")
            }
            Self::ArchivedEntries { entries, .. } => {
                write!(f, "{} ledger entries have been archived", entries.len())
            }
            Self::FootprintUnavailable { detail } => {
                write!(f, "ledger footprint unavailable: {detail}")
            }
            Self::LedgerAdvanced {
                simulated_ledger,
                current_ledger,
            } => write!(
                f,
                "ledger advanced from {simulated_ledger} to {current_ledger} during simulation"
            ),
            Self::Rpc(err) => write!(f, "simulation failed: {err}"),
            Self::MalformedResponse { detail } => {
                write!(f, "malformed simulation response: {detail}")
            }
        }
    }
}

impl std::error::Error for SimulationError {}

impl From<RpcError> for SimulationError {
    fn from(error: RpcError) -> Self {
        Self::Rpc(error)
    }
}

/// The last ledger a footprint simulated at `simulated_ledger` is valid for.
pub fn footprint_valid_until(simulated_ledger: u32, ttl_ledgers: u32) -> u32 {
    simulated_ledger.saturating_add(ttl_ledgers)
}

/// Compute the resource fee from the resources the simulation declared.
///
/// The fee is derived from the declared instructions, bytes read, bytes written
/// and events — never from a hard-coded amount — and is never below the
/// `minResourceFee` the node reported.
pub fn resource_fee_for(
    resources: &SimulateResources,
    min_resource_fee: i64,
    pricing: &ResourcePricing,
) -> i64 {
    let declared = resources.instructions as i64 * pricing.fee_per_instruction
        + resources.read_bytes as i64 * pricing.fee_per_read_byte
        + resources.write_bytes as i64 * pricing.fee_per_write_byte
        + resources.events as i64 * pricing.fee_per_event;
    declared.max(min_resource_fee).max(0)
}

/// A stable hash of a ledger footprint.
///
/// The hash is recorded with the transaction so an auditor can tell whether
/// two transactions declared the same state without storing the footprint
/// itself.
pub fn footprint_hash(footprint: &SimulateFootprint) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"zenith-footprint-v1");
    hasher.update(b"read:");
    for key in &footprint.read {
        hasher.update(key.xdr.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"write:");
    for key in &footprint.write {
        hasher.update(key.xdr.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Extract the contract error code out of a raw host error.
///
/// Soroban encodes contract failures as `Error(Contract, #<code>)`; anything
/// else is reported without a code.
pub fn parse_contract_error_code(raw: &str) -> Option<u32> {
    raw.split('#')
        .nth(1)
        .and_then(|rest| rest.trim_end_matches(')').trim().parse::<u32>().ok())
}

/// Runs simulations against a node and turns the answers into something the
/// transaction builder can use.
#[derive(Clone)]
pub struct SorobanSimulator {
    rpc: Arc<dyn SorobanRpc>,
}

impl SorobanSimulator {
    /// Create a simulator backed by the given node.
    pub fn new(rpc: Arc<dyn SorobanRpc>) -> Self {
        Self { rpc }
    }

    /// The node this simulator talks to.
    pub fn rpc(&self) -> &Arc<dyn SorobanRpc> {
        &self.rpc
    }

    /// The latest closed ledger, if the node can be reached.
    pub async fn current_ledger(&self) -> Result<u32, SimulationError> {
        self.rpc.latest_ledger().await.map_err(SimulationError::Rpc)
    }

    /// Simulate an envelope and extract the footprint and resources.
    pub async fn simulate(
        &self,
        envelope_xdr: &str,
    ) -> Result<SimulatedTransaction, SimulationError> {
        let response = self
            .rpc
            .simulate_transaction(envelope_xdr)
            .await
            .map_err(SimulationError::Rpc)?;
        Self::interpret(response)
    }

    /// Simulate, re-simulating while the chain advances under the simulation.
    ///
    /// The simulation is repeated at most `max_attempts` times; when the chain
    /// keeps advancing the caller gets a [`SimulationError::LedgerAdvanced`].
    pub async fn simulate_until_stable(
        &self,
        envelope_xdr: &str,
        ttl_ledgers: u32,
        max_attempts: u32,
    ) -> Result<SimulatedTransaction, SimulationError> {
        let mut attempts = 0u32;
        loop {
            let simulated = self.simulate(envelope_xdr).await?;
            let current = self.current_ledger().await?;
            if simulated.footprint_is_valid(current, ttl_ledgers) {
                return Ok(simulated);
            }
            if attempts >= max_attempts {
                return Err(SimulationError::LedgerAdvanced {
                    simulated_ledger: simulated.simulated_ledger,
                    current_ledger: current,
                });
            }
            attempts += 1;
        }
    }

    /// Turn a raw simulation response into a [`SimulatedTransaction`].
    pub fn interpret(
        response: SimulateTransactionResponse,
    ) -> Result<SimulatedTransaction, SimulationError> {
        if let Some(error) = response.error.clone() {
            return Err(SimulationError::ContractError {
                code: parse_contract_error_code(&error),
                message: error,
            });
        }

        let transaction_data = response.transaction_data.clone().ok_or_else(|| {
            SimulationError::MalformedResponse {
                detail: "simulation did not return transaction data".to_string(),
            }
        })?;

        let resources = response.resources.clone().unwrap_or_default();

        if resources.restore_footprint_required {
            let restore_footprint = resources.restore_footprint.clone();
            let entries = restore_footprint
                .as_ref()
                .map(|footprint| {
                    footprint
                        .read
                        .iter()
                        .chain(footprint.write.iter())
                        .map(|key| key.xdr.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            return Err(SimulationError::ArchivedEntries {
                entries,
                restore_footprint,
            });
        }

        if resources.footprint.is_none() {
            return Err(SimulationError::FootprintUnavailable {
                detail: "simulation did not report a ledger footprint".to_string(),
            });
        }

        let min_resource_fee = response
            .min_resource_fee
            .as_deref()
            .and_then(|raw| raw.trim().parse::<i64>().ok())
            .unwrap_or(0);

        Ok(SimulatedTransaction {
            transaction_data,
            min_resource_fee,
            resources,
            simulated_ledger: response.latest_ledger.unwrap_or(0),
            restore_footprint: None,
        })
    }
}

/// A simulation provider that can be injected into the transaction builder.
#[async_trait]
pub trait SimulationProvider: Send + Sync {
    /// Simulate an envelope and return the footprint and resources.
    async fn simulate(&self, envelope_xdr: &str) -> Result<SimulatedTransaction, SimulationError>;
}

#[async_trait]
impl SimulationProvider for SorobanSimulator {
    async fn simulate(&self, envelope_xdr: &str) -> Result<SimulatedTransaction, SimulationError> {
        SorobanSimulator::simulate(self, envelope_xdr).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::types::LedgerKey;

    fn footprint() -> SimulateFootprint {
        SimulateFootprint {
            read: vec![LedgerKey {
                xdr: "AAAA-READ-1".into(),
            }],
            write: vec![LedgerKey {
                xdr: "AAAA-WRITE-1".into(),
            }],
        }
    }

    fn response() -> SimulateTransactionResponse {
        SimulateTransactionResponse {
            transaction_data: Some("AAAA_TX_DATA".into()),
            min_resource_fee: Some("1000".into()),
            results: Vec::new(),
            latest_ledger: Some(500),
            error: None,
            resources: Some(SimulateResources {
                instructions: 1_000,
                read_bytes: 200,
                write_bytes: 100,
                events: 2,
                footprint: Some(footprint()),
                restore_footprint_required: false,
                restore_footprint: None,
            }),
        }
    }

    #[test]
    fn extracts_the_footprint_and_resources() {
        let simulated = SorobanSimulator::interpret(response()).unwrap();
        assert_eq!(simulated.min_resource_fee, 1_000);
        assert_eq!(simulated.simulated_ledger, 500);
        let footprint = simulated.footprint().unwrap();
        assert_eq!(footprint.read.len(), 1);
        assert_eq!(footprint.write.len(), 1);
        assert_eq!(simulated.resources.instructions, 1_000);
    }

    #[test]
    fn resource_fee_follows_the_declared_resources() {
        let resources = response().resources.unwrap();
        let pricing = ResourcePricing::default();
        // 1000 instructions + 200 read bytes + 100 write bytes + 2 events.
        assert_eq!(resource_fee_for(&resources, 0, &pricing), 1_302);
        // The node's minimum is a lower bound.
        assert_eq!(resource_fee_for(&resources, 5_000, &pricing), 5_000);
    }

    #[test]
    fn contract_errors_are_typed() {
        let mut response = response();
        response.error = Some("HostError: Error(Contract, #12)".into());
        let err = SorobanSimulator::interpret(response).unwrap_err();
        assert_eq!(err.code(), "CONTRACT_ERROR");
        assert_eq!(
            err,
            SimulationError::ContractError {
                code: Some(12),
                message: "HostError: Error(Contract, #12)".into()
            }
        );
    }

    #[test]
    fn archived_entries_are_reported_with_the_restore_footprint() {
        let mut response = response();
        let mut resources = response.resources.clone().unwrap();
        resources.restore_footprint_required = true;
        resources.restore_footprint = Some(footprint());
        response.resources = Some(resources);

        let err = SorobanSimulator::interpret(response).unwrap_err();
        assert_eq!(err.code(), "ARCHIVED_ENTRIES");
        match err {
            SimulationError::ArchivedEntries { entries, .. } => assert_eq!(entries.len(), 2),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_missing_footprint_is_reported() {
        let mut response = response();
        response.resources = None;
        let err = SorobanSimulator::interpret(response).unwrap_err();
        assert_eq!(err.code(), "FOOTPRINT_UNAVAILABLE");
    }

    #[test]
    fn footprints_expire_after_the_ttl() {
        let simulated = SorobanSimulator::interpret(response()).unwrap();
        assert!(simulated.footprint_is_valid(502, DEFAULT_FOOTPRINT_TTL_LEDGERS));
        assert!(!simulated.footprint_is_valid(503, DEFAULT_FOOTPRINT_TTL_LEDGERS));
    }

    #[test]
    fn the_footprint_hash_is_stable_and_order_sensitive() {
        let first = footprint_hash(&footprint());
        assert_eq!(first, footprint_hash(&footprint()));
        assert_eq!(first.len(), 64);

        let mut swapped = footprint();
        swapped.read = vec![LedgerKey {
            xdr: "AAAA-WRITE-1".into(),
        }];
        swapped.write = vec![LedgerKey {
            xdr: "AAAA-READ-1".into(),
        }];
        assert_ne!(first, footprint_hash(&swapped));
    }

    #[test]
    fn contract_error_codes_are_parsed() {
        assert_eq!(parse_contract_error_code("Error(Contract, #12)"), Some(12));
        assert_eq!(
            parse_contract_error_code("Error(WasmVm, MissingValue)"),
            None
        );
    }
}
