use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FixedMarginLeg { pub quantity: i128, pub base_value: i128, pub shocked_value: i128 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedMarginRequirement { pub initial: i128, pub maintenance: i128, pub scenario_index: usize }

pub fn calculate(scenarios: &[Vec<FixedMarginLeg>], initial_multiplier_bps: i128) -> Result<FixedMarginRequirement, String> {
    if scenarios.is_empty() || initial_multiplier_bps < 10_000 { return Err("invalid margin scenario configuration".into()); }
    let mut worst_loss = 0i128;
    let mut scenario_index = 0;
    for (index, legs) in scenarios.iter().enumerate() {
        let loss = legs.iter().try_fold(0i128, |total, leg| {
            let movement = leg.shocked_value.checked_sub(leg.base_value).ok_or("margin subtraction overflow")?;
            let pnl = movement.checked_mul(leg.quantity).ok_or("margin multiplication overflow")?;
            total.checked_sub(pnl).ok_or("margin accumulation overflow")
        })?.max(0);
        if loss > worst_loss { worst_loss = loss; scenario_index = index; }
    }
    let initial = worst_loss.checked_mul(initial_multiplier_bps).ok_or("initial margin overflow")? / 10_000;
    Ok(FixedMarginRequirement { initial, maintenance: worst_loss, scenario_index })
}
