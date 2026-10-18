use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind { MissingEvent, ExtraEvent, AmountMismatch }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drift { pub kind: DriftKind, pub key: String, pub expected: Option<i128>, pub actual: Option<i128>, pub replay_from_ledger: u32, pub replay_to_ledger: u32 }

pub fn snapshot_hash(values: &BTreeMap<String, i128>) -> String {
    let mut hasher = Sha256::new();
    for (key, value) in values { hasher.update(format!("{key}:{value}\n")); }
    format!("{:x}", hasher.finalize())
}

pub fn compare(expected: &BTreeMap<String, i128>, actual: &BTreeMap<String, i128>, ledger: u32, previous_ledger: u32) -> Vec<Drift> {
    let mut drifts = Vec::new();
    for (key, expected_value) in expected {
        match actual.get(key) {
            None => drifts.push(Drift { kind: DriftKind::MissingEvent, key: key.clone(), expected: Some(*expected_value), actual: None, replay_from_ledger: previous_ledger, replay_to_ledger: ledger }),
            Some(actual_value) if actual_value != expected_value => drifts.push(Drift { kind: DriftKind::AmountMismatch, key: key.clone(), expected: Some(*expected_value), actual: Some(*actual_value), replay_from_ledger: previous_ledger, replay_to_ledger: ledger }),
            _ => {}
        }
    }
    for (key, actual_value) in actual { if !expected.contains_key(key) { drifts.push(Drift { kind: DriftKind::ExtraEvent, key: key.clone(), expected: None, actual: Some(*actual_value), replay_from_ledger: previous_ledger, replay_to_ledger: ledger }); } }
    drifts
}
