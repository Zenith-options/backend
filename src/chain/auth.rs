//! Validation and collection of opaque Soroban authorization entries.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedAuthEntry {
    pub signer: String,
    pub invocation_hash: [u8; 32],
    pub expiration_ledger: u64,
    pub xdr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthCollectionError {
    ExpiredBatch,
    UnexpectedSigner,
    InvocationMismatch,
    ExpirationTooSoon,
    DuplicateSigner,
    ReplayedEntry,
}

#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub batch_id: String,
    pub invocation_hash: [u8; 32],
    pub expiration_ledger: u64,
    pub collection_deadline_unix: i64,
    required_signers: BTreeSet<String>,
    entries: BTreeMap<String, SignedAuthEntry>,
    fingerprints: HashSet<[u8; 32]>,
}

impl PendingAuthorization {
    pub fn prepare(
        batch_id: impl Into<String>,
        invocation_hash: [u8; 32],
        expiration_ledger: u64,
        collection_deadline_unix: i64,
        required_signers: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            batch_id: batch_id.into(),
            invocation_hash,
            expiration_ledger,
            collection_deadline_unix,
            required_signers: required_signers.into_iter().collect(),
            entries: BTreeMap::new(),
            fingerprints: HashSet::new(),
        }
    }

    pub fn unsigned_payload(&self, signer: &str) -> Option<(&[u8; 32], u64)> {
        self.required_signers.contains(signer).then_some((&self.invocation_hash, self.expiration_ledger))
    }

    pub fn collect(
        &mut self,
        entry: SignedAuthEntry,
        current_ledger: u64,
        now_unix: i64,
        minimum_ledger_margin: u64,
    ) -> Result<(), AuthCollectionError> {
        if now_unix >= self.collection_deadline_unix { return Err(AuthCollectionError::ExpiredBatch); }
        if !self.required_signers.contains(&entry.signer) { return Err(AuthCollectionError::UnexpectedSigner); }
        if entry.invocation_hash != self.invocation_hash { return Err(AuthCollectionError::InvocationMismatch); }
        if entry.expiration_ledger < current_ledger.saturating_add(minimum_ledger_margin)
            || entry.expiration_ledger > self.expiration_ledger { return Err(AuthCollectionError::ExpirationTooSoon); }
        if self.entries.contains_key(&entry.signer) { return Err(AuthCollectionError::DuplicateSigner); }
        let fingerprint: [u8; 32] = Sha256::digest(&entry.xdr).into();
        if !self.fingerprints.insert(fingerprint) { return Err(AuthCollectionError::ReplayedEntry); }
        self.entries.insert(entry.signer.clone(), entry);
        Ok(())
    }

    pub fn is_ready(&self) -> bool { self.entries.len() == self.required_signers.len() }

    pub fn ordered_entries(&self) -> Option<Vec<&SignedAuthEntry>> {
        self.is_ready().then(|| self.entries.values().collect())
    }
}
