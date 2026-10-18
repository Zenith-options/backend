use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Clone, Serialize, Deserialize, Eq)]
pub struct LiquidationCandidate { pub wallet: String, pub collateral: i128, pub maintenance_margin: i128, pub oracle_ledger: u32 }

impl LiquidationCandidate { pub fn shortfall(&self) -> i128 { self.maintenance_margin.saturating_sub(self.collateral).max(0) } }
impl PartialEq for LiquidationCandidate { fn eq(&self, other: &Self) -> bool { self.wallet == other.wallet && self.shortfall() == other.shortfall() } }
impl Ord for LiquidationCandidate { fn cmp(&self, other: &Self) -> Ordering { self.shortfall().cmp(&other.shortfall()).then_with(|| other.wallet.cmp(&self.wallet)) } }
impl PartialOrd for LiquidationCandidate { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }

pub fn eligible(candidate: &LiquidationCandidate, current_ledger: u32, max_oracle_age_ledgers: u32) -> Result<bool, String> {
    if current_ledger.saturating_sub(candidate.oracle_ledger) > max_oracle_age_ledgers { return Err("oracle is stale; liquidation halted".into()); }
    Ok(candidate.collateral < candidate.maintenance_margin)
}

pub fn bounded_batch(mut candidates: Vec<LiquidationCandidate>, notional_cap: i128) -> Vec<LiquidationCandidate> {
    candidates.sort_by(|left, right| right.cmp(left));
    let mut used = 0i128;
    candidates.into_iter().take_while(|candidate| { let next = used.saturating_add(candidate.shortfall()); if next > notional_cap { false } else { used = next; true } }).collect()
}
