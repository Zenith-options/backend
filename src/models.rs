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
    pub strategy_id: Option<String>,
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

/// Health state of an account with respect to its margin requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountHealth {
    /// Equity is at or above the initial margin requirement.
    Healthy,
    /// Equity is between the maintenance and initial margin requirements.
    MarginCall,
    /// Equity has fallen below the maintenance margin requirement.
    Liquidating,
}

impl AccountHealth {
    /// Classifies an account given its equity and margin requirements.
    ///
    /// `initial_margin` is the requirement above which the account is healthy;
    /// `maintenance_margin` is the floor below which liquidation is triggered.
    pub fn classify(equity: f64, initial_margin: f64, maintenance_margin: f64) -> Self {
        if equity < maintenance_margin {
            AccountHealth::Liquidating
        } else if equity < initial_margin {
            AccountHealth::MarginCall
        } else {
            AccountHealth::Healthy
        }
    }
}

/// A single liquidation event recorded for an under-margined account.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Liquidation {
    pub id: String,
    pub wallet_address: String,
    /// JSON-encoded list of the position ids closed in this liquidation.
    pub positions_closed: String,
    /// JSON-encoded list of the prices used to close each position.
    pub prices: String,
    /// Liquidation penalty charged, in the account's balance units.
    pub penalty: f64,
    /// Account equity before the liquidation ran.
    pub pre_equity: f64,
    /// Account equity after the liquidation ran.
    pub post_equity: f64,
    /// Health state before the liquidation ran.
    pub pre_health: String,
    /// Health state after the liquidation ran.
    pub post_health: String,
    /// Bad debt absorbed by the insurance fund, if equity went negative.
    pub bad_debt: f64,
    pub created_at: String,
}
