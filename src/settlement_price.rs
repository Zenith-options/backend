use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceObservation { pub ledger: u32, pub price: i128, pub source: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementPrice { pub price: i128, pub from_ledger: u32, pub to_ledger: u32, pub source: String, pub observations: Vec<PriceObservation>, pub dispute_deadline_ledger: u32, pub challenged: bool }

pub fn determine(mut primary: Vec<PriceObservation>, mut secondary: Vec<PriceObservation>, from_ledger: u32, to_ledger: u32, minimum: i128, maximum: i128, dispute_ledgers: u32) -> Result<SettlementPrice, String> {
    let normalize = |observations: &mut Vec<PriceObservation>| { observations.retain(|observation| observation.ledger >= from_ledger && observation.ledger <= to_ledger && observation.price >= minimum && observation.price <= maximum); observations.sort_by_key(|observation| observation.ledger); let mut by_ledger = BTreeMap::new(); for observation in observations.drain(..) { by_ledger.insert(observation.ledger, observation); } *observations = by_ledger.into_values().collect(); };
    normalize(&mut primary); normalize(&mut secondary);
    let (observations, source) = if !primary.is_empty() { (primary, "primary_twap") } else if !secondary.is_empty() { (secondary, "secondary_twap") } else { return Err("settlement deferred: no bounded oracle observations".into()); };
    let sum = observations.iter().try_fold(0i128, |total, observation| total.checked_add(observation.price).ok_or("settlement TWAP overflow"))?;
    let price = sum / observations.len() as i128;
    Ok(SettlementPrice { price, from_ledger, to_ledger, source: source.into(), observations, dispute_deadline_ledger: to_ledger.saturating_add(dispute_ledgers), challenged: false })
}

impl SettlementPrice {
    pub fn challenge(&mut self, current_ledger: u32) -> Result<(), String> { if current_ledger > self.dispute_deadline_ledger { return Err("dispute window closed".into()); } if self.challenged { return Err("settlement already challenged".into()); } self.challenged = true; Ok(()) }
    pub fn may_finalize(&self, current_ledger: u32) -> bool { !self.challenged && current_ledger > self.dispute_deadline_ledger }
}
