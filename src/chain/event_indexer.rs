use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedEvent { pub contract_id: String, pub ledger: u32, pub transaction_hash: String, pub event_index: u32, pub topics: Vec<String>, pub raw_xdr: String, pub decoded_payload: Option<serde_json::Value>, pub decode_error: Option<String> }

#[derive(Debug, Clone)]
pub struct EventBatch { pub from_ledger: u32, pub to_ledger: u32, pub events: Vec<IndexedEvent>, pub next_cursor: String }

pub fn next_batch(cursor_ledger: u32, latest_ledger: u32, batch_size: u32, overlap: u32) -> Option<(u32, u32)> {
    if cursor_ledger >= latest_ledger { return None; }
    let from = cursor_ledger.saturating_sub(overlap).saturating_add(1);
    Some((from, from.saturating_add(batch_size.saturating_sub(1)).min(latest_ledger)))
}

pub fn detect_gap(expected_ledger: u32, events: &[IndexedEvent]) -> Option<(u32, u32)> {
    events.iter().map(|event| event.ledger).min().filter(|first| *first > expected_ledger).map(|first| (expected_ledger, first - 1))
}

pub trait EventStore {
    fn commit_batch_and_cursor(&mut self, batch: EventBatch) -> Result<(), String>;
}
