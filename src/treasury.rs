use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreasuryEntry { pub category: String, pub amount_minor: i128, pub timestamp: i64 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreasuryReport {
    pub period_start: i64,
    pub period_end: i64,
    pub categories: BTreeMap<String, i128>,
    pub ledger_total_minor: i128,
    pub finalized_at: Option<i64>,
}

impl TreasuryReport {
    pub fn build(period_start: i64, period_end: i64, entries: &[TreasuryEntry], ledger_total_minor: i128) -> Self {
        let mut categories = BTreeMap::new();
        for entry in entries.iter().filter(|entry| entry.timestamp >= period_start && entry.timestamp < period_end) {
            *categories.entry(entry.category.clone()).or_insert(0) += entry.amount_minor;
        }
        Self { period_start, period_end, categories, ledger_total_minor, finalized_at: None }
    }

    pub fn finalize(&mut self, timestamp: i64) -> Result<(), String> {
        let computed: i128 = self.categories.values().sum();
        if computed != self.ledger_total_minor { return Err(format!("treasury discrepancy: report={computed}, ledger={}", self.ledger_total_minor)); }
        if self.finalized_at.is_some() { return Err("finalized reports are immutable; create an adjusting entry".into()); }
        self.finalized_at = Some(timestamp);
        Ok(())
    }

    pub fn runway_months(&self, liquid_holdings_minor: i128, monthly_expenses_minor: i128) -> Option<f64> {
        (monthly_expenses_minor > 0).then(|| liquid_holdings_minor as f64 / monthly_expenses_minor as f64)
    }
}
