//! Canonical fixed-point payout rounding and dust accounting policy.

/// Every payout is rounded down at the protocol's integer settlement scale.
/// The exact remainder is retained as attributed dust, so value is conserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub payouts: Vec<u128>,
    pub dust: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMathError {
    EmptyWeights,
    ZeroWeight,
    Overflow,
}

/// Allocate `pool_debit` pro rata, suppressing individual payouts below
/// `minimum_payout`. Suppressed amounts and division remainders become dust.
pub fn allocate_pro_rata(
    pool_debit: u128,
    weights: &[u128],
    minimum_payout: u128,
) -> Result<Allocation, SettlementMathError> {
    if weights.is_empty() {
        return Err(SettlementMathError::EmptyWeights);
    }
    let total_weight = weights.iter().try_fold(0_u128, |sum, value| {
        sum.checked_add(*value).ok_or(SettlementMathError::Overflow)
    })?;
    if total_weight == 0 {
        return Err(SettlementMathError::ZeroWeight);
    }

    let mut paid = 0_u128;
    let mut payouts = Vec::with_capacity(weights.len());
    for weight in weights {
        let raw = pool_debit
            .checked_mul(*weight)
            .ok_or(SettlementMathError::Overflow)?
            / total_weight;
        let payout = if raw < minimum_payout { 0 } else { raw };
        paid = paid.checked_add(payout).ok_or(SettlementMathError::Overflow)?;
        payouts.push(payout);
    }

    Ok(Allocation { payouts, dust: pool_debit - paid })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DustSweep {
    pub amount: u128,
    pub treasury: String,
    pub ledger: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DustAccount {
    balance: u128,
    pub sweep_threshold: u128,
    pub max_ledgers_between_sweeps: u64,
    pub last_sweep_ledger: u64,
}

impl DustAccount {
    pub fn new(sweep_threshold: u128, max_ledgers_between_sweeps: u64) -> Self {
        Self { balance: 0, sweep_threshold, max_ledgers_between_sweeps, last_sweep_ledger: 0 }
    }

    pub fn balance(&self) -> u128 { self.balance }

    pub fn accrue(&mut self, amount: u128) -> Result<(), SettlementMathError> {
        self.balance = self.balance.checked_add(amount).ok_or(SettlementMathError::Overflow)?;
        Ok(())
    }

    pub fn sweep_if_due(&mut self, ledger: u64, treasury: impl Into<String>) -> Option<DustSweep> {
        let age_due = ledger.saturating_sub(self.last_sweep_ledger) >= self.max_ledgers_between_sweeps;
        if self.balance == 0 || (self.balance < self.sweep_threshold && !age_due) {
            return None;
        }
        let amount = std::mem::take(&mut self.balance);
        self.last_sweep_ledger = ledger;
        Some(DustSweep { amount, treasury: treasury.into(), ledger })
    }
}
