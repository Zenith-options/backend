//! Atomic prepare -> collect -> assemble boundary for settlement batches.

use super::auth::{PendingAuthorization, SignedAuthEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledBatch {
    pub batch_id: String,
    pub invocation_hash: [u8; 32],
    pub authorization_xdr: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchAssemblyError {
    Incomplete,
}

/// Assembly has no side effects: sequence consumption and submission happen
/// only after this returns a complete transaction payload.
pub fn assemble(pending: &PendingAuthorization) -> Result<AssembledBatch, BatchAssemblyError> {
    let entries: Vec<&SignedAuthEntry> = pending.ordered_entries().ok_or(BatchAssemblyError::Incomplete)?;
    Ok(AssembledBatch {
        batch_id: pending.batch_id.clone(),
        invocation_hash: pending.invocation_hash,
        authorization_xdr: entries.into_iter().map(|entry| entry.xdr.clone()).collect(),
    })
}
