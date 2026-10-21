//! Event catalogue: every settlement-relevant lifecycle event a trader can be
//! notified about, its severity, and the template that renders it.
//!
//! Templates only ever see [`TemplateData`], a closed set of typed,
//! sanitised fields. There is deliberately no free-form "message" or "error"
//! field, so internal error details, secrets or other accounts' balances
//! cannot leak into a notification body by construction.

use serde::{Deserialize, Serialize};

/// How urgent an event is. Drives defaults, quiet hours and mute behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    /// Funds are at risk or action is required. Never muted, never deferred
    /// by quiet hours, never rate-capped.
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// Delivery channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    InApp,
    Email,
    Push,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::InApp, Channel::Email, Channel::Push];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::InApp => "in_app",
            Channel::Email => "email",
            Channel::Push => "push",
        }
    }

    pub fn parse(s: &str) -> Option<Channel> {
        Channel::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// The lifecycle event catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventType {
    /// Expiry is `hours_before` hours away (T-N reminder).
    ExpiryReminder {
        hours_before: u32,
    },
    SettlementStarted,
    SettlementCompleted,
    SettlementFailed,
    PayoutCredited,
    MarginCall,
    LiquidationWarning,
    RolloverExecuted,
}

impl EventType {
    /// Every catalogue entry, with a representative reminder horizon.
    pub const CATALOGUE: [EventType; 8] = [
        EventType::ExpiryReminder { hours_before: 24 },
        EventType::SettlementStarted,
        EventType::SettlementCompleted,
        EventType::SettlementFailed,
        EventType::PayoutCredited,
        EventType::MarginCall,
        EventType::LiquidationWarning,
        EventType::RolloverExecuted,
    ];

    /// Stable key, used in the dedupe key and in preference overrides.
    /// Reminders include their horizon so T-24h and T-1h are distinct events.
    pub fn key(self) -> String {
        match self {
            EventType::ExpiryReminder { hours_before } => {
                format!("expiry_reminder_{hours_before}h")
            }
            other => other.family().to_string(),
        }
    }

    /// Key without the reminder horizon: preference overrides are set per
    /// family ("expiry_reminder"), not per horizon.
    pub fn family(self) -> &'static str {
        match self {
            EventType::ExpiryReminder { .. } => "expiry_reminder",
            EventType::SettlementStarted => "settlement_started",
            EventType::SettlementCompleted => "settlement_completed",
            EventType::SettlementFailed => "settlement_failed",
            EventType::PayoutCredited => "payout_credited",
            EventType::MarginCall => "margin_call",
            EventType::LiquidationWarning => "liquidation_warning",
            EventType::RolloverExecuted => "rollover_executed",
        }
    }

    pub fn parse_family(s: &str) -> Option<EventType> {
        EventType::CATALOGUE.into_iter().find(|e| e.family() == s)
    }

    pub fn severity(self) -> Severity {
        match self {
            EventType::ExpiryReminder { hours_before } if hours_before <= 1 => Severity::Warning,
            EventType::ExpiryReminder { .. } => Severity::Info,
            EventType::SettlementStarted => Severity::Info,
            EventType::SettlementCompleted => Severity::Info,
            EventType::PayoutCredited => Severity::Info,
            EventType::RolloverExecuted => Severity::Info,
            EventType::SettlementFailed => Severity::Critical,
            EventType::MarginCall => Severity::Critical,
            EventType::LiquidationWarning => Severity::Critical,
        }
    }
}

/// Public, user-safe reason a settlement failed. Internal errors are mapped
/// to one of these before they reach the notification layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicFailureReason {
    /// Retrying automatically; no action needed.
    Retrying,
    /// The wallet needs a trustline for the settlement asset.
    MissingTrustline,
    /// Escalated to the operations team.
    ManualReview,
}

impl PublicFailureReason {
    fn sentence(self) -> &'static str {
        match self {
            PublicFailureReason::Retrying => "We are retrying automatically; no action is needed.",
            PublicFailureReason::MissingTrustline => {
                "Your wallet needs a trustline for the settlement asset before funds can be delivered."
            }
            PublicFailureReason::ManualReview => "Our team has been alerted and is reviewing it.",
        }
    }
}

/// The only data a template may interpolate. Every string is sanitised by
/// [`sanitize`] before rendering.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateData {
    /// Instrument label, e.g. `XLM-27DEC26-0.12-C`.
    pub instrument: Option<String>,
    /// Pre-formatted amount owned by *this* user, e.g. `125.50`.
    pub amount: Option<String>,
    /// Asset code, e.g. `USDC`.
    pub asset: Option<String>,
    /// Pre-formatted deadline, e.g. `2026-12-27 08:00 UTC`.
    pub deadline: Option<String>,
    /// Instrument a position was rolled into.
    pub rolled_into: Option<String>,
    pub failure_reason: Option<PublicFailureReason>,
}

/// A rendered, channel-agnostic message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedMessage {
    pub title: String,
    pub body: String,
}

const MAX_FIELD_LEN: usize = 64;

/// Sanitise an interpolated field: keep a conservative allow-list of
/// characters (so no markup, control characters or template syntax gets
/// through), collapse anything else to nothing, and cap the length.
pub fn sanitize(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | ',' | ':' | '/' | '+')
        })
        .take(MAX_FIELD_LEN)
        .collect();
    cleaned.trim().to_string()
}

fn field(v: &Option<String>, fallback: &str) -> String {
    v.as_deref()
        .map(sanitize)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Render the template for `event` from sanitised `data`.
pub fn render(event: EventType, data: &TemplateData) -> RenderedMessage {
    let instrument = field(&data.instrument, "your position");
    let amount = field(&data.amount, "");
    let asset = field(&data.asset, "");
    let money = format!("{amount} {asset}").trim().to_string();
    let deadline = field(&data.deadline, "soon");

    let (title, body) = match event {
        EventType::ExpiryReminder { hours_before } => (
            format!("{instrument} expires in {hours_before}h"),
            format!("{instrument} expires in {hours_before} hours. Open positions settle automatically at expiry."),
        ),
        EventType::SettlementStarted => (
            "Settlement started".to_string(),
            format!("Settlement of {instrument} has started."),
        ),
        EventType::SettlementCompleted => (
            "Settlement completed".to_string(),
            format!("{instrument} has been settled."),
        ),
        EventType::SettlementFailed => (
            "Settlement needs attention".to_string(),
            format!(
                "Settlement of {instrument} did not complete. {}",
                data.failure_reason.unwrap_or(PublicFailureReason::ManualReview).sentence()
            ),
        ),
        EventType::PayoutCredited => (
            "Payout credited".to_string(),
            if money.is_empty() {
                format!("Your payout for {instrument} has been credited.")
            } else {
                format!("{money} has been credited to your account for {instrument}.")
            },
        ),
        EventType::MarginCall => (
            "Margin call".to_string(),
            if money.is_empty() {
                format!("Your account needs more collateral by {deadline} to keep {instrument} open.")
            } else {
                format!("Add {money} of collateral by {deadline} to keep {instrument} open.")
            },
        ),
        EventType::LiquidationWarning => (
            "Liquidation warning".to_string(),
            format!("{instrument} is close to liquidation. Add collateral or reduce the position before {deadline}."),
        ),
        EventType::RolloverExecuted => (
            "Rollover executed".to_string(),
            format!("{instrument} was rolled into {}.", field(&data.rolled_into, "a new series")),
        ),
    };
    RenderedMessage { title, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_covers_required_events_with_unique_keys() {
        let keys: std::collections::HashSet<_> =
            EventType::CATALOGUE.iter().map(|e| e.family()).collect();
        assert_eq!(keys.len(), EventType::CATALOGUE.len());
        for e in EventType::CATALOGUE {
            assert_eq!(
                EventType::parse_family(e.family()).map(|x| x.family()),
                Some(e.family())
            );
        }
    }

    #[test]
    fn reminder_keys_include_horizon() {
        assert_eq!(
            EventType::ExpiryReminder { hours_before: 24 }.key(),
            "expiry_reminder_24h"
        );
        assert_eq!(
            EventType::ExpiryReminder { hours_before: 1 }.key(),
            "expiry_reminder_1h"
        );
        assert_eq!(
            EventType::ExpiryReminder { hours_before: 1 }.severity(),
            Severity::Warning
        );
    }

    #[test]
    fn funds_at_risk_events_are_critical() {
        for e in [
            EventType::MarginCall,
            EventType::LiquidationWarning,
            EventType::SettlementFailed,
        ] {
            assert_eq!(e.severity(), Severity::Critical);
        }
    }

    #[test]
    fn sanitize_strips_markup_and_caps_length() {
        assert_eq!(sanitize("<script>alert(1)</script>"), "scriptalert1/script");
        assert_eq!(sanitize("{{secret}}\n\r\t"), "secret");
        assert_eq!(sanitize(&"A".repeat(500)).len(), MAX_FIELD_LEN);
    }

    #[test]
    fn render_uses_only_sanitised_fields() {
        let data = TemplateData {
            instrument: Some("XLM-27DEC26-0.12-C<b>".into()),
            amount: Some("125.50".into()),
            asset: Some("USDC".into()),
            ..Default::default()
        };
        let msg = render(EventType::PayoutCredited, &data);
        assert_eq!(
            msg.body,
            "125.50 USDC has been credited to your account for XLM-27DEC26-0.12-Cb."
        );
        assert!(!msg.body.contains('<'));
    }

    #[test]
    fn failure_template_never_carries_internal_detail() {
        let msg = render(EventType::SettlementFailed, &TemplateData::default());
        assert!(msg.body.contains("Our team has been alerted"));
    }
}
