use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct AuthorizationPolicy { pub enabled: bool, pub allowed_contracts: HashSet<String>, pub allowed_methods: HashSet<String>, pub maximum_fee: u32, pub maximum_amount_per_transaction: i128, pub maximum_amount_per_window: i128 }

#[derive(Debug, Clone)]
pub struct PrivilegedAction { pub contract_id: String, pub method: String, pub asset: String, pub amount: i128, pub fee: u32 }

impl AuthorizationPolicy {
    pub fn authorize(&self, action: &PrivilegedAction, window_spend: &HashMap<String, i128>) -> Result<(), &'static str> {
        if !self.enabled { return Err("settlement kill switch is active"); }
        if !self.allowed_contracts.contains(&action.contract_id) || !self.allowed_methods.contains(&action.method) { return Err("contract invocation is not allowlisted"); }
        if action.fee > self.maximum_fee { return Err("fee exceeds policy ceiling"); }
        if action.amount <= 0 || action.amount > self.maximum_amount_per_transaction { return Err("amount exceeds per-transaction limit"); }
        if window_spend.get(&action.asset).copied().unwrap_or(0).saturating_add(action.amount) > self.maximum_amount_per_window { return Err("rolling spend limit exceeded"); }
        Ok(())
    }
}
