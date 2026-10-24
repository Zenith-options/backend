use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardAllocation { pub wallet: String, pub amount_minor: i128, pub category: String, pub justification: Option<String> }

pub fn attribute_referral(referrer: &str, referee: &str, existing: Option<&str>) -> Result<String, String> {
    if existing.is_some() { return Err("referral attribution is immutable".into()); }
    if referrer == referee { return Err("self-referral is not allowed".into()); }
    Ok(referrer.to_owned())
}

pub fn finalize_epoch(allocations: &[RewardAllocation]) -> (BTreeMap<String, i128>, String) {
    let mut totals = BTreeMap::new();
    for allocation in allocations { *totals.entry(allocation.wallet.clone()).or_insert(0) += allocation.amount_minor; }
    let mut hasher = Sha256::new();
    for (wallet, amount) in &totals { hasher.update(format!("{wallet}:{amount}\n")); }
    (totals, format!("{:x}", hasher.finalize()))
}

pub fn manual_reward(wallet: String, amount_minor: i128, category: String, justification: String) -> Result<RewardAllocation, String> {
    if justification.trim().is_empty() { return Err("manual rewards require justification".into()); }
    Ok(RewardAllocation { wallet, amount_minor, category, justification: Some(justification) })
}
