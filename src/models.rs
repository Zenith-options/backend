use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Account {
    pub wallet_address: String,
    pub balance: f64,
    pub collateral_locked: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Position {
    pub id: String,
    pub wallet_address: String,
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String,
    pub position_type: String,
    pub contracts: f64,
    pub entry_premium: f64,
    pub entry_spot: f64,
    pub collateral: f64,
    pub status: String,
    pub close_premium: Option<f64>,
    pub close_spot: Option<f64>,
    pub realized_pnl: Option<f64>,
    pub opened_at: String,
    pub closed_at: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct WatchlistItem {
    pub wallet_address: String,
    pub underlying: String,
    pub added_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Alert {
    pub id: String,
    pub wallet_address: String,
    pub underlying: String,
    pub condition: String,
    pub target_price: f64,
    pub triggered: bool,
    pub created_at: String,
    pub triggered_at: Option<String>,
}
