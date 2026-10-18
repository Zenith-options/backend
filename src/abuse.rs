use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingEvent {
    pub id: String,
    pub wallet: String,
    pub timestamp: i64,
    pub kind: String,
    pub profit_minor: i64,
    pub price_age_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: &'static str,
    pub evidence: Vec<String>,
}

pub trait DetectionRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn evaluate(&self, events: &[TradingEvent]) -> Vec<Finding>;
}

pub struct ThresholdRules {
    pub stale_price_seconds: u64,
    pub event_burst: usize,
    pub profit_concentration_minor: i64,
}

impl DetectionRule for ThresholdRules {
    fn name(&self) -> &'static str { "threshold_rules" }

    fn evaluate(&self, events: &[TradingEvent]) -> Vec<Finding> {
        let mut findings = Vec::new();
        let stale: Vec<_> = events.iter().filter(|event| event.price_age_seconds >= self.stale_price_seconds).map(|event| event.id.clone()).collect();
        if !stale.is_empty() { findings.push(Finding { rule: "stale-price-exploitation", severity: "high", evidence: stale }); }
        let mut wallets: HashMap<&str, Vec<&TradingEvent>> = HashMap::new();
        for event in events { wallets.entry(&event.wallet).or_default().push(event); }
        for wallet_events in wallets.values() {
            if wallet_events.len() >= self.event_burst { findings.push(Finding { rule: "coordinated-rate-evasion", severity: "medium", evidence: wallet_events.iter().map(|event| event.id.clone()).collect() }); }
            if wallet_events.iter().map(|event| event.profit_minor).sum::<i64>() >= self.profit_concentration_minor { findings.push(Finding { rule: "profit-concentration", severity: "medium", evidence: wallet_events.iter().map(|event| event.id.clone()).collect() }); }
            let kinds: HashSet<_> = wallet_events.iter().map(|event| event.kind.as_str()).collect();
            if kinds.contains("open") && kinds.contains("close") { findings.push(Finding { rule: "rapid-open-close-loop", severity: "low", evidence: wallet_events.iter().map(|event| event.id.clone()).collect() }); }
        }
        if wallets.len() > 1 && events.len() >= self.event_burst { findings.push(Finding { rule: "multi-wallet-cluster", severity: "medium", evidence: events.iter().map(|event| event.id.clone()).collect() }); }
        findings
    }
}

pub fn backtest(rule: &dyn DetectionRule, windows: &[Vec<TradingEvent>]) -> f64 {
    if windows.is_empty() { return 0.0; }
    windows.iter().filter(|window| !rule.evaluate(window).is_empty()).count() as f64 / windows.len() as f64
}
