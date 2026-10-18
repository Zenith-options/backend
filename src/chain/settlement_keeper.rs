use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettlementJobState { Pending, Simulating, Submitted, Confirmed, Failed }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementJob { pub id: String, pub series_id: String, pub expiry_ledger: u32, pub state: SettlementJobState, pub attempts: u32, pub transaction_hash: Option<String> }

impl SettlementJob {
    pub fn new(series_id: String, expiry_ledger: u32) -> Self {
        let id = format!("{:x}", Sha256::digest(format!("{series_id}:{expiry_ledger}")));
        Self { id, series_id, expiry_ledger, state: SettlementJobState::Pending, attempts: 0, transaction_hash: None }
    }

    pub fn advance(&mut self, next: SettlementJobState) -> Result<(), String> {
        let valid = matches!((&self.state, &next),
            (SettlementJobState::Pending, SettlementJobState::Simulating) |
            (SettlementJobState::Simulating, SettlementJobState::Submitted) |
            (SettlementJobState::Simulating, SettlementJobState::Failed) |
            (SettlementJobState::Submitted, SettlementJobState::Confirmed) |
            (SettlementJobState::Submitted, SettlementJobState::Failed) |
            (SettlementJobState::Failed, SettlementJobState::Pending));
        if !valid { return Err("invalid settlement state transition".into()); }
        if matches!(next, SettlementJobState::Simulating) { self.attempts += 1; }
        self.state = next;
        Ok(())
    }
}

pub fn due_for_settlement(current_ledger: u32, expiry_ledger: u32, open_interest: i128) -> bool { open_interest > 0 && current_ledger >= expiry_ledger }
