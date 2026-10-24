use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub seq: i64,
    pub timestamp: i64,
    pub actor: String,
    pub action: String,
    pub subject: String,
    pub payload_json: String,
    pub previous_hash: String,
    pub hash: String,
}

impl AuditEntry {
    pub fn append(seq: i64, timestamp: i64, actor: String, action: String, subject: String, payload_json: String, previous_hash: String) -> Self {
        let canonical = format!("{seq}\n{timestamp}\n{actor}\n{action}\n{subject}\n{payload_json}\n{previous_hash}");
        let hash = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        Self { seq, timestamp, actor, action, subject, payload_json, previous_hash, hash }
    }
}

pub fn verify_chain(entries: &[AuditEntry], starting_hash: &str) -> Result<(), i64> {
    let mut previous = starting_hash.to_owned();
    for entry in entries {
        let expected = AuditEntry::append(entry.seq, entry.timestamp, entry.actor.clone(), entry.action.clone(), entry.subject.clone(), entry.payload_json.clone(), previous.clone());
        if entry.previous_hash != previous || entry.hash != expected.hash {
            return Err(entry.seq);
        }
        previous = entry.hash.clone();
    }
    Ok(())
}

pub trait CheckpointSink: Send + Sync {
    fn anchor(&self, sequence: i64, hash: &str) -> Result<(), String>;
}
