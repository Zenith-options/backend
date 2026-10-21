//! Per-user notification preferences and their resolution rules.
//!
//! Resolution order for one `(event, channel)` pair:
//!
//! 1. **Critical in-app is mandatory.** A critical event (margin call,
//!    liquidation warning, settlement failed) always lands in the in-app
//!    inbox; it cannot be muted or switched off there.
//! 2. **Explicit override wins.** `overrides["<family>:<channel>"]` turns an
//!    event family on or off for one channel.
//! 3. **Global mute** suppresses every *non-critical* event on every channel.
//!    Critical events still go out on the channels that are enabled for them.
//! 4. **Defaults** (see [`default_enabled`]) apply when there is no override.
//! 5. **Quiet hours** defer non-critical email/push until the window ends.
//!    In-app is never deferred (the inbox is passive) and critical events are
//!    never deferred.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::catalogue::{Channel, EventType, Severity};

const MINUTES_PER_DAY: i64 = 24 * 60;
const MS_PER_MINUTE: i64 = 60_000;

/// A daily quiet window in the user's local time. `start == end` disables it.
/// The window may wrap midnight (e.g. 22:00 → 07:00).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// Minutes after local midnight, `0..1440`.
    pub start_minute: u16,
    /// Minutes after local midnight, `0..1440`. Exclusive.
    pub end_minute: u16,
    /// Offset of the user's local time from UTC, in minutes.
    pub utc_offset_minutes: i16,
}

impl QuietHours {
    pub fn is_valid(&self) -> bool {
        (self.start_minute as i64) < MINUTES_PER_DAY
            && (self.end_minute as i64) < MINUTES_PER_DAY
            && (self.utc_offset_minutes as i64).abs() <= 14 * 60
    }

    fn local_minute(&self, now_ms: i64) -> i64 {
        (now_ms.div_euclid(MS_PER_MINUTE) + self.utc_offset_minutes as i64)
            .rem_euclid(MINUTES_PER_DAY)
    }

    /// If `now_ms` falls inside the window, the epoch-ms instant it ends.
    pub fn deferral_until(&self, now_ms: i64) -> Option<i64> {
        let (start, end) = (self.start_minute as i64, self.end_minute as i64);
        if start == end {
            return None;
        }
        let m = self.local_minute(now_ms);
        let inside = if start < end {
            m >= start && m < end
        } else {
            m >= start || m < end
        };
        if !inside {
            return None;
        }
        let minutes_left = (end - m).rem_euclid(MINUTES_PER_DAY);
        let minute_floor = now_ms.div_euclid(MS_PER_MINUTE) * MS_PER_MINUTE;
        Some(minute_floor + minutes_left * MS_PER_MINUTE)
    }
}

/// A user's stored preferences. `Default` is the out-of-the-box behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPreferences {
    #[serde(default)]
    pub global_mute: bool,
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    /// `"<event family>:<channel>" -> enabled`.
    #[serde(default)]
    pub overrides: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreferenceError {
    UnknownEvent(String),
    UnknownChannel(String),
    InvalidQuietHours,
}

impl std::fmt::Display for PreferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreferenceError::UnknownEvent(e) => write!(f, "unknown event type `{e}`"),
            PreferenceError::UnknownChannel(c) => write!(f, "unknown channel `{c}`"),
            PreferenceError::InvalidQuietHours => write!(f, "quiet hours out of range"),
        }
    }
}

impl std::error::Error for PreferenceError {}

fn override_key(event: EventType, channel: Channel) -> String {
    format!("{}:{}", event.family(), channel.as_str())
}

impl UserPreferences {
    pub fn set_override(&mut self, event: EventType, channel: Channel, enabled: bool) {
        self.overrides.insert(override_key(event, channel), enabled);
    }

    /// Reject unknown keys and out-of-range quiet hours, e.g. from an API body.
    pub fn validate(&self) -> Result<(), PreferenceError> {
        for key in self.overrides.keys() {
            let (family, channel) = key
                .split_once(':')
                .ok_or_else(|| PreferenceError::UnknownEvent(key.clone()))?;
            EventType::parse_family(family)
                .ok_or_else(|| PreferenceError::UnknownEvent(family.to_string()))?;
            Channel::parse(channel)
                .ok_or_else(|| PreferenceError::UnknownChannel(channel.to_string()))?;
        }
        match self.quiet_hours {
            Some(q) if !q.is_valid() => Err(PreferenceError::InvalidQuietHours),
            _ => Ok(()),
        }
    }

    fn override_for(&self, event: EventType, channel: Channel) -> Option<bool> {
        self.overrides.get(&override_key(event, channel)).copied()
    }
}

/// Out-of-the-box channel defaults.
///
/// * In-app: everything.
/// * Email: anything warning-or-worse, plus money-movement outcomes
///   (settlement completed, payout credited, rollover executed).
/// * Push: anything critical, plus expiry reminders and payouts.
pub fn default_enabled(event: EventType, channel: Channel) -> bool {
    match channel {
        Channel::InApp => true,
        Channel::Email => {
            event.severity() >= Severity::Warning
                || matches!(
                    event,
                    EventType::SettlementCompleted
                        | EventType::PayoutCredited
                        | EventType::RolloverExecuted
                )
        }
        Channel::Push => {
            event.severity() == Severity::Critical
                || matches!(
                    event,
                    EventType::ExpiryReminder { .. } | EventType::PayoutCredited
                )
        }
    }
}

/// Why a notification was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressReason {
    OptedOut,
    Muted,
    DisabledByDefault,
    RateCapped,
}

impl SuppressReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SuppressReason::OptedOut => "opted_out",
            SuppressReason::Muted => "muted",
            SuppressReason::DisabledByDefault => "disabled_by_default",
            SuppressReason::RateCapped => "rate_capped",
        }
    }
}

/// Outcome of preference resolution for one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deliver,
    /// Deliver, but not before this epoch-ms instant (quiet hours).
    Defer {
        until_ms: i64,
    },
    Suppress(SuppressReason),
}

/// Resolve whether `event` should go out on `channel` at `now_ms`.
pub fn resolve(
    prefs: &UserPreferences,
    event: EventType,
    channel: Channel,
    now_ms: i64,
) -> Decision {
    let critical = event.severity() == Severity::Critical;

    if critical && channel == Channel::InApp {
        return Decision::Deliver;
    }

    let enabled = match prefs.override_for(event, channel) {
        Some(false) => return Decision::Suppress(SuppressReason::OptedOut),
        Some(true) => true,
        None => default_enabled(event, channel),
    };

    if prefs.global_mute && !critical {
        return Decision::Suppress(SuppressReason::Muted);
    }
    if !enabled {
        return Decision::Suppress(SuppressReason::DisabledByDefault);
    }

    if !critical && channel != Channel::InApp {
        if let Some(until_ms) = prefs.quiet_hours.and_then(|q| q.deferral_until(now_ms)) {
            return Decision::Defer { until_ms };
        }
    }
    Decision::Deliver
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 3_600_000;
    /// 2026-10-21T00:00:00Z.
    const MIDNIGHT: i64 = 1_792_540_800_000;

    #[test]
    fn defaults_follow_documented_matrix() {
        let p = UserPreferences::default();
        assert_eq!(
            resolve(&p, EventType::SettlementStarted, Channel::InApp, MIDNIGHT),
            Decision::Deliver
        );
        assert_eq!(
            resolve(&p, EventType::SettlementStarted, Channel::Email, MIDNIGHT),
            Decision::Suppress(SuppressReason::DisabledByDefault)
        );
        assert_eq!(
            resolve(&p, EventType::PayoutCredited, Channel::Email, MIDNIGHT),
            Decision::Deliver
        );
        assert_eq!(
            resolve(&p, EventType::MarginCall, Channel::Push, MIDNIGHT),
            Decision::Deliver
        );
        assert_eq!(
            resolve(
                &p,
                EventType::ExpiryReminder { hours_before: 24 },
                Channel::Push,
                MIDNIGHT
            ),
            Decision::Deliver
        );
    }

    #[test]
    fn global_mute_spares_critical_events() {
        let p = UserPreferences {
            global_mute: true,
            ..Default::default()
        };
        assert_eq!(
            resolve(&p, EventType::PayoutCredited, Channel::InApp, MIDNIGHT),
            Decision::Suppress(SuppressReason::Muted)
        );
        assert_eq!(
            resolve(&p, EventType::LiquidationWarning, Channel::Email, MIDNIGHT),
            Decision::Deliver
        );
        assert_eq!(
            resolve(&p, EventType::LiquidationWarning, Channel::InApp, MIDNIGHT),
            Decision::Deliver
        );
    }

    #[test]
    fn overrides_enable_and_disable() {
        let mut p = UserPreferences::default();
        p.set_override(EventType::SettlementStarted, Channel::Email, true);
        p.set_override(EventType::PayoutCredited, Channel::Push, false);
        assert_eq!(
            resolve(&p, EventType::SettlementStarted, Channel::Email, MIDNIGHT),
            Decision::Deliver
        );
        assert_eq!(
            resolve(&p, EventType::PayoutCredited, Channel::Push, MIDNIGHT),
            Decision::Suppress(SuppressReason::OptedOut)
        );
        // A reminder override applies to every horizon of that family.
        p.set_override(
            EventType::ExpiryReminder { hours_before: 24 },
            Channel::Push,
            false,
        );
        assert_eq!(
            resolve(
                &p,
                EventType::ExpiryReminder { hours_before: 1 },
                Channel::Push,
                MIDNIGHT
            ),
            Decision::Suppress(SuppressReason::OptedOut)
        );
    }

    #[test]
    fn critical_in_app_cannot_be_disabled() {
        let mut p = UserPreferences {
            global_mute: true,
            ..Default::default()
        };
        p.set_override(EventType::MarginCall, Channel::InApp, false);
        assert_eq!(
            resolve(&p, EventType::MarginCall, Channel::InApp, MIDNIGHT),
            Decision::Deliver
        );
        // ...but the user may opt out of the external copy.
        p.set_override(EventType::MarginCall, Channel::Email, false);
        assert_eq!(
            resolve(&p, EventType::MarginCall, Channel::Email, MIDNIGHT),
            Decision::Suppress(SuppressReason::OptedOut)
        );
    }

    #[test]
    fn quiet_hours_defer_non_critical_external_channels_only() {
        // 22:00 -> 07:00 local, user at UTC+2. 01:00Z is 03:00 local: inside.
        let p = UserPreferences {
            quiet_hours: Some(QuietHours {
                start_minute: 22 * 60,
                end_minute: 7 * 60,
                utc_offset_minutes: 120,
            }),
            ..Default::default()
        };
        let now = MIDNIGHT + HOUR_MS; // 03:00 local
        let until = MIDNIGHT + 5 * HOUR_MS; // 07:00 local = 05:00Z
        assert_eq!(
            resolve(&p, EventType::PayoutCredited, Channel::Email, now),
            Decision::Defer { until_ms: until }
        );
        assert_eq!(
            resolve(&p, EventType::PayoutCredited, Channel::InApp, now),
            Decision::Deliver
        );
        assert_eq!(
            resolve(&p, EventType::MarginCall, Channel::Push, now),
            Decision::Deliver
        );
        // 06:00Z is 08:00 local: outside the window.
        assert_eq!(
            resolve(
                &p,
                EventType::PayoutCredited,
                Channel::Email,
                MIDNIGHT + 6 * HOUR_MS
            ),
            Decision::Deliver
        );
    }

    #[test]
    fn quiet_hours_non_wrapping_and_disabled() {
        let q = QuietHours {
            start_minute: 60,
            end_minute: 120,
            utc_offset_minutes: 0,
        };
        assert_eq!(
            q.deferral_until(MIDNIGHT + 90 * 60_000 + 15_000),
            Some(MIDNIGHT + 2 * HOUR_MS)
        );
        assert_eq!(q.deferral_until(MIDNIGHT + 2 * HOUR_MS), None);
        let off = QuietHours {
            start_minute: 60,
            end_minute: 60,
            utc_offset_minutes: 0,
        };
        assert_eq!(off.deferral_until(MIDNIGHT + 60 * 60_000), None);
    }

    #[test]
    fn validate_rejects_unknown_keys() {
        let mut p = UserPreferences::default();
        p.overrides.insert("bogus:email".into(), true);
        assert!(matches!(
            p.validate(),
            Err(PreferenceError::UnknownEvent(_))
        ));
        let mut p = UserPreferences::default();
        p.overrides.insert("margin_call:sms".into(), true);
        assert!(matches!(
            p.validate(),
            Err(PreferenceError::UnknownChannel(_))
        ));
        let p = UserPreferences {
            quiet_hours: Some(QuietHours {
                start_minute: 2000,
                end_minute: 0,
                utc_offset_minutes: 0,
            }),
            ..Default::default()
        };
        assert_eq!(p.validate(), Err(PreferenceError::InvalidQuietHours));
    }
}
