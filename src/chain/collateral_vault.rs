use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VaultIntentKind { Deposit, Withdraw }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultIntent { pub id: String, pub wallet: String, pub asset: String, pub amount: i128, pub kind: VaultIntentKind, pub nonce: String, pub expires_at: i64, pub confirmed_ledger: Option<u32> }

pub fn create_intent(wallet: String, asset: String, amount: i128, kind: VaultIntentKind, now: i64, ttl_seconds: i64, allowed_assets: &[String], free_collateral: i128, required_collateral: i128) -> Result<VaultIntent, String> {
    if amount <= 0 { return Err("amount must be positive".into()); }
    if !allowed_assets.contains(&asset) { return Err("asset is not allowlisted".into()); }
    if kind == VaultIntentKind::Withdraw && free_collateral.saturating_sub(amount) < required_collateral { return Err("withdrawal would breach required collateral".into()); }
    Ok(VaultIntent { id: Uuid::new_v4().to_string(), wallet, asset, amount, kind, nonce: Uuid::new_v4().to_string(), expires_at: now.saturating_add(ttl_seconds), confirmed_ledger: None })
}

pub fn apply_confirmed_event(intent: &mut VaultIntent, ledger: u32) -> Result<(), String> {
    if intent.confirmed_ledger.is_some() { return Err("vault intent already confirmed".into()); }
    intent.confirmed_ledger = Some(ledger);
    Ok(())
}
