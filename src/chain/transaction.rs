use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation { pub contract_id: String, pub function: String, pub arguments_xdr: Vec<String> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedEnvelope {
    pub source: String,
    pub sequence: i64,
    pub network_passphrase: String,
    pub inclusion_fee: u32,
    pub resource_fee: u32,
    pub invocation: Invocation,
    pub footprint_xdr: String,
    pub auth_xdr: Vec<String>,
    pub valid_until_ledger: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResult { pub footprint_xdr: String, pub auth_xdr: Vec<String>, pub minimum_resource_fee: u32, pub latest_ledger: u32, pub archived_entries: Vec<String> }

#[derive(Debug)]
pub enum TransactionError { Simulation(String), FeeCeiling { required: u32, maximum: u32 }, StaleSimulation, Signing(String), Submission(String) }

#[derive(Default)]
pub struct SequenceAllocator { next: Mutex<HashMap<String, i64>> }

impl SequenceAllocator {
    pub fn allocate(&self, source: &str, current_network_sequence: i64) -> i64 {
        let mut next = self.next.lock().unwrap();
        let allocated = next.get(source).copied().unwrap_or(current_network_sequence + 1).max(current_network_sequence + 1);
        next.insert(source.to_owned(), allocated + 1);
        allocated
    }
}

pub fn assemble(invocation: Invocation, source: String, sequence: i64, network_passphrase: String, base_fee: u32, maximum_fee: u32, simulation: SimulationResult) -> Result<PreparedEnvelope, TransactionError> {
    let fee = base_fee.checked_add(simulation.minimum_resource_fee).ok_or(TransactionError::FeeCeiling { required: u32::MAX, maximum: maximum_fee })?;
    if fee > maximum_fee { return Err(TransactionError::FeeCeiling { required: fee, maximum: maximum_fee }); }
    if !simulation.archived_entries.is_empty() { return Err(TransactionError::Simulation("restore archived footprint before rebuilding".into())); }
    Ok(PreparedEnvelope { source, sequence, network_passphrase, inclusion_fee: base_fee, resource_fee: simulation.minimum_resource_fee, invocation, footprint_xdr: simulation.footprint_xdr, auth_xdr: simulation.auth_xdr, valid_until_ledger: simulation.latest_ledger.saturating_add(5) })
}

pub trait EnvelopeSigner { fn key_id(&self) -> &str; fn sign(&self, envelope: &PreparedEnvelope) -> Result<Vec<u8>, TransactionError>; }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedEnvelope { pub transaction_hash: String, pub envelope_xdr: String, pub signer_key_id: String }

pub fn sign(envelope: &PreparedEnvelope, signer: &dyn EnvelopeSigner, current_ledger: u32) -> Result<SignedEnvelope, TransactionError> {
    if current_ledger > envelope.valid_until_ledger { return Err(TransactionError::StaleSimulation); }
    let signature = signer.sign(envelope)?;
    let encoded = serde_json::to_string(envelope).map_err(|error| TransactionError::Signing(error.to_string()))?;
    let mut hasher = Sha256::new(); hasher.update(encoded.as_bytes()); hasher.update(&signature);
    Ok(SignedEnvelope { transaction_hash: format!("{:x}", hasher.finalize()), envelope_xdr: encoded, signer_key_id: signer.key_id().into() })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionOutcome { Pending(String), Duplicate(String), TryAgainLater, Rejected { result_xdr: Option<String> } }

pub fn fee_bump(inner: SignedEnvelope, sponsor: String, fee: u32, ceiling: u32) -> Result<(SignedEnvelope, String, u32), TransactionError> {
    if fee > ceiling { return Err(TransactionError::FeeCeiling { required: fee, maximum: ceiling }); }
    Ok((inner, sponsor, fee))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationStatus { Pending, Success, Failed, ExpiredUnknown }

pub fn next_poll_delay(attempt: u32, maximum_seconds: u64) -> u64 { 2u64.saturating_pow(attempt.min(10)).min(maximum_seconds) }
