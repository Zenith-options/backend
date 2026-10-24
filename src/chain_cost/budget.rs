//! Budget policy: per-category, per-period limits with warn and hard-stop
//! thresholds.
//!
//! | State      | Committed (spent + reserved)       | Non-critical traffic                      | Critical traffic |
//! |------------|------------------------------------|-------------------------------------------|------------------|
//! | `ok`       | below `warn_ratio` × limit         | allowed                                   | allowed          |
//! | `warn`     | at or above warn, up to the limit  | throttled to `throttle_per_minute`        | allowed          |
//! | `hard_stop`| any submission would exceed limit  | **rejected**                              | allowed, alarmed |
//!
//! Periods are UTC: daily periods start at 00:00, weekly periods start on
//! Monday at 00:00.

use serde::{Deserialize, Serialize};

use super::model::BusinessAction;

pub const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Daily,
    Weekly,
}

impl Period {
    pub fn as_str(self) -> &'static str {
        match self {
            Period::Daily => "daily",
            Period::Weekly => "weekly",
        }
    }

    pub fn parse(s: &str) -> Option<Period> {
        match s {
            "daily" => Some(Period::Daily),
            "weekly" => Some(Period::Weekly),
            _ => None,
        }
    }

    /// Start (epoch ms) of the period containing `now_ms`.
    pub fn start_ms(self, now_ms: i64) -> i64 {
        let day = now_ms.div_euclid(DAY_MS);
        match self {
            Period::Daily => day * DAY_MS,
            // 1970-01-01 was a Thursday; shift so Monday is weekday 0.
            Period::Weekly => (day - (day + 3).rem_euclid(7)) * DAY_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetPolicy {
    pub category: BusinessAction,
    pub period: Period,
    pub limit_stroops: i64,
    /// Warn threshold in basis points of the limit (8000 = 80%).
    pub warn_ratio_bps: u32,
    /// Non-critical reservations allowed per minute while in `warn`.
    pub throttle_per_minute: u32,
}

impl BudgetPolicy {
    pub fn warn_threshold(&self) -> i64 {
        (self.limit_stroops as i128 * self.warn_ratio_bps.min(10_000) as i128 / 10_000) as i64
    }

    pub fn state(&self, committed: i64) -> BudgetState {
        if committed >= self.limit_stroops {
            BudgetState::HardStop
        } else if committed >= self.warn_threshold() {
            BudgetState::Warn
        } else {
            BudgetState::Ok
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetState {
    Ok,
    Warn,
    HardStop,
}

impl BudgetState {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetState::Ok => "ok",
            BudgetState::Warn => "warn",
            BudgetState::HardStop => "hard_stop",
        }
    }

    /// Gauge value for metrics: 0 = ok, 1 = warn, 2 = hard stop.
    pub fn gauge(self) -> u8 {
        self as u8
    }
}

/// Snapshot of one category's budget in the current period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetStatus {
    pub category: BusinessAction,
    pub period: Period,
    pub period_start_ms: i64,
    pub limit_stroops: i64,
    pub spent: i64,
    pub reserved: i64,
    /// `(spent + reserved) / limit`, in basis points.
    pub utilization_bps: i64,
    pub state: BudgetState,
}

impl BudgetStatus {
    pub fn new(policy: &BudgetPolicy, period_start_ms: i64, spent: i64, reserved: i64) -> Self {
        let committed = spent + reserved;
        let utilization_bps = if policy.limit_stroops == 0 {
            if committed > 0 {
                i64::MAX
            } else {
                0
            }
        } else {
            (committed as i128 * 10_000 / policy.limit_stroops as i128) as i64
        };
        Self {
            category: policy.category,
            period: policy.period,
            period_start_ms,
            limit_stroops: policy.limit_stroops,
            spent,
            reserved,
            utilization_bps,
            state: policy.state(committed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_boundaries_are_utc_days_and_monday_weeks() {
        // 2026-10-21 is a Wednesday.
        let wed_noon = 20_747 * DAY_MS + DAY_MS / 2;
        assert_eq!(Period::Daily.start_ms(wed_noon), 20_747 * DAY_MS);
        assert_eq!(Period::Weekly.start_ms(wed_noon), 20_745 * DAY_MS);
        // A Monday maps to itself.
        assert_eq!(Period::Weekly.start_ms(20_745 * DAY_MS), 20_745 * DAY_MS);
        // 1970-01-05 was the first Monday after the epoch.
        assert_eq!(Period::Weekly.start_ms(4 * DAY_MS + 1), 4 * DAY_MS);
        assert_eq!(Period::Weekly.start_ms(0), -3 * DAY_MS);
    }

    #[test]
    fn thresholds_drive_state() {
        let p = BudgetPolicy {
            category: BusinessAction::KeeperRun,
            period: Period::Daily,
            limit_stroops: 1_000,
            warn_ratio_bps: 8_000,
            throttle_per_minute: 5,
        };
        assert_eq!(p.state(799), BudgetState::Ok);
        assert_eq!(p.state(800), BudgetState::Warn);
        assert_eq!(p.state(1_000), BudgetState::HardStop);
        let s = BudgetStatus::new(&p, 0, 600, 300);
        assert_eq!((s.utilization_bps, s.state), (9_000, BudgetState::Warn));
    }
}
