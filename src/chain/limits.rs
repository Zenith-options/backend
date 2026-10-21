//! Atomic exposure-limit state machine mirrored by the chain client.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskLimits {
    pub version: u64,
    pub effective_ledger: u64,
    pub halt_new_writes: bool,
    pub global_notional: u128,
    pub per_account_notional: Option<u128>,
    pub underlying_open_interest: HashMap<String, u128>,
    pub series_open_interest: HashMap<String, u128>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureState {
    pub global_notional: u128,
    pub by_account: HashMap<String, u128>,
    pub by_underlying: HashMap<String, u128>,
    pub by_series: HashMap<String, u128>,
}

impl Default for ExposureState {
    fn default() -> Self {
        Self { global_notional: 0, by_account: HashMap::new(), by_underlying: HashMap::new(), by_series: HashMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitError {
    Halted,
    NotEffective,
    GlobalNotionalExceeded,
    AccountNotionalExceeded,
    UnderlyingOpenInterestExceeded,
    SeriesOpenInterestExceeded,
    ArithmeticOverflow,
    Unauthorized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitUpdateEvent {
    pub previous_version: u64,
    pub new_version: u64,
    pub effective_ledger: u64,
    pub administrator: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionWrite<'a> {
    pub account: &'a str,
    pub underlying: &'a str,
    pub series: &'a str,
    pub notional: u128,
    pub open_interest: u128,
}

impl RiskLimits {
    pub fn authorize_update(
        current: &Self,
        mut next: Self,
        caller: &str,
        administrator: &str,
    ) -> Result<(Self, LimitUpdateEvent), LimitError> {
        if caller != administrator { return Err(LimitError::Unauthorized); }
        next.version = current.version.checked_add(1).ok_or(LimitError::ArithmeticOverflow)?;
        let event = LimitUpdateEvent {
            previous_version: current.version,
            new_version: next.version,
            effective_ledger: next.effective_ledger,
            administrator: administrator.to_owned(),
        };
        Ok((next, event))
    }

    pub fn check_and_apply(
        &self,
        state: &mut ExposureState,
        write: PositionWrite<'_>,
        ledger: u64,
    ) -> Result<(), LimitError> {
        if ledger < self.effective_ledger { return Err(LimitError::NotEffective); }
        if self.halt_new_writes { return Err(LimitError::Halted); }

        let global = checked_add(state.global_notional, write.notional)?;
        if global > self.global_notional { return Err(LimitError::GlobalNotionalExceeded); }
        let account = checked_add(*state.by_account.get(write.account).unwrap_or(&0), write.notional)?;
        if self.per_account_notional.is_some_and(|cap| account > cap) { return Err(LimitError::AccountNotionalExceeded); }
        let underlying = checked_add(*state.by_underlying.get(write.underlying).unwrap_or(&0), write.open_interest)?;
        if self.underlying_open_interest.get(write.underlying).is_some_and(|cap| underlying > *cap) {
            return Err(LimitError::UnderlyingOpenInterestExceeded);
        }
        let series = checked_add(*state.by_series.get(write.series).unwrap_or(&0), write.open_interest)?;
        if self.series_open_interest.get(write.series).is_some_and(|cap| series > *cap) {
            return Err(LimitError::SeriesOpenInterestExceeded);
        }

        state.global_notional = global;
        state.by_account.insert(write.account.to_owned(), account);
        state.by_underlying.insert(write.underlying.to_owned(), underlying);
        state.by_series.insert(write.series.to_owned(), series);
        Ok(())
    }

    pub fn release(state: &mut ExposureState, write: PositionWrite<'_>) {
        state.global_notional = state.global_notional.saturating_sub(write.notional);
        subtract(&mut state.by_account, write.account, write.notional);
        subtract(&mut state.by_underlying, write.underlying, write.open_interest);
        subtract(&mut state.by_series, write.series, write.open_interest);
    }
}

fn checked_add(left: u128, right: u128) -> Result<u128, LimitError> {
    left.checked_add(right).ok_or(LimitError::ArithmeticOverflow)
}

fn subtract(map: &mut HashMap<String, u128>, key: &str, amount: u128) {
    let remaining = map.get(key).copied().unwrap_or(0).saturating_sub(amount);
    if remaining == 0 { map.remove(key); } else { map.insert(key.to_owned(), remaining); }
}
