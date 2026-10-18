use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainReadModel<T> { pub data: T, pub as_of_ledger: u32, pub stale: bool }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionView { pub hash: String, pub status: String, pub ledger: Option<u32>, pub error: Option<String>, pub submitted_at: i64, pub updated_at: i64 }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultBalanceView { pub wallet: String, pub free: i128, pub locked: i128 }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarginView { pub wallet: String, pub initial: i128, pub maintenance: i128, pub collateral: i128 }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidationView { pub id: String, pub wallet: String, pub reason: String, pub transaction_hash: String, pub ledger: u32 }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletChainEvent { pub wallet: String, pub cursor: String, pub ledger: u32, pub kind: String, pub payload: serde_json::Value }

pub fn authorize_wallet(authenticated_wallet: &str, requested_wallet: &str) -> Result<(), &'static str> {
    if authenticated_wallet == requested_wallet { Ok(()) } else { Err("forbidden") }
}

pub fn is_stale(indexed_ledger: u32, latest_ledger: u32, threshold: u32) -> bool { latest_ledger.saturating_sub(indexed_ledger) > threshold }
