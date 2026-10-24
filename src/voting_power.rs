use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

pub fn resolve_delegate<'a>(address: &'a str, delegations: &'a HashMap<String, String>, max_depth: usize) -> Result<&'a str, String> {
    let mut current = address;
    let mut seen = HashSet::new();
    for _ in 0..=max_depth {
        if !seen.insert(current) { return Err("delegation cycle detected".into()); }
        match delegations.get(current) { Some(next) => current = next, None => return Ok(current) }
    }
    Err("delegation depth exceeded".into())
}

pub fn snapshot_root(powers: &BTreeMap<String, i128>) -> String {
    let mut leaves: Vec<Vec<u8>> = powers.iter().map(|(address, power)| Sha256::digest(format!("{address}:{power}").as_bytes()).to_vec()).collect();
    if leaves.is_empty() { return format!("{:x}", Sha256::digest([])); }
    while leaves.len() > 1 {
        if leaves.len() % 2 == 1 { leaves.push(leaves.last().cloned().unwrap_or_default()); }
        leaves = leaves.chunks(2).map(|pair| { let mut bytes = pair[0].clone(); bytes.extend_from_slice(&pair[1]); Sha256::digest(bytes).to_vec() }).collect();
    }
    leaves[0].iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn time_weighted_power(balance: i128, lock_start: i64, lock_end: i64, ledger_time: i64, max_lock_seconds: i64) -> i128 {
    if ledger_time >= lock_end || max_lock_seconds <= 0 { return 0; }
    let remaining = (lock_end - ledger_time).min(max_lock_seconds).max(0) as i128;
    let base = if ledger_time >= lock_start { balance } else { 0 };
    base.saturating_mul(remaining) / max_lock_seconds as i128
}
