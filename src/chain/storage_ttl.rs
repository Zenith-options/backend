use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageKey { pub key_xdr: String, pub contract_id: String, pub durability: String, pub live_until_ledger: u32, pub active: bool, pub last_bump_ledger: Option<u32> }

pub fn days_to_ledgers(days: u32, observed_close_seconds: u32) -> Result<u32, String> {
    if observed_close_seconds == 0 { return Err("observed ledger close time must be positive".into()); }
    (days as u64).checked_mul(86_400).and_then(|seconds| seconds.checked_div(observed_close_seconds as u64)).and_then(|ledgers| u32::try_from(ledgers).ok()).ok_or_else(|| "TTL ledger conversion overflow".into())
}

pub fn extension_plan(keys: &[StorageKey], current_ledger: u32, floor_ledgers: u32, ceiling_ledgers: u32, network_maximum: u32, maximum_batch_size: usize) -> BTreeMap<String, Vec<Vec<StorageKey>>> {
    let target = ceiling_ledgers.min(network_maximum);
    let mut grouped: BTreeMap<String, Vec<StorageKey>> = BTreeMap::new();
    for key in keys.iter().filter(|key| key.active && key.live_until_ledger.saturating_sub(current_ledger) < floor_ledgers && key.last_bump_ledger != Some(current_ledger)) { let mut planned = key.clone(); planned.live_until_ledger = current_ledger.saturating_add(target); grouped.entry(key.contract_id.clone()).or_default().push(planned); }
    grouped.into_iter().map(|(contract, keys)| (contract, keys.chunks(maximum_batch_size.max(1)).map(|batch| batch.to_vec()).collect())).collect()
}
