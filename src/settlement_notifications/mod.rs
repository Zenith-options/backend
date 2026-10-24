//! User-facing settlement lifecycle notifications (issue #229).
//!
//! Traders are told when something happens to their money: expiry is near,
//! settlement started/completed/failed, a payout landed, a margin call or
//! liquidation warning fired, or a rollover executed. Delivery goes through
//! in-app, email and push, governed by per-user preferences and
//! deduplicated per channel.
//!
//! * [`catalogue`]: event types, severities and sanitised templates.
//! * [`preferences`]: defaults, overrides, global mute, quiet hours.
//! * [`channels`]: the [`channels::ChannelProvider`] trait and transports.
//! * [`dispatch`]: transactional outbox, worker, retries, attempt log.
//!
//! Wiring: the settlement engine maps each event on its stream to a
//! [`LifecycleSignal`] and calls [`notify`] with its open transaction. A
//! background job calls [`dispatch::Dispatcher::run_once`] on an interval.
//! See `docs/notifications.md` for the full catalogue and preference rules.

pub mod catalogue;
pub mod channels;
pub mod dispatch;
pub mod preferences;

pub use catalogue::{Channel, EventType, PublicFailureReason, Severity, TemplateData};
pub use dispatch::{enqueue, Dispatcher, EnqueueReport, NotificationEvent, RateCap, RetryPolicy};
pub use preferences::{Decision, QuietHours, UserPreferences};

/// Settlement-layer events that produce trader notifications. Amounts are
/// pre-formatted strings for the *receiving* user only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleSignal {
    ExpiryApproaching {
        user_id: String,
        position_id: String,
        instrument: String,
        hours_before: u32,
    },
    SettlementStarted {
        user_id: String,
        position_id: String,
        instrument: String,
    },
    SettlementCompleted {
        user_id: String,
        position_id: String,
        instrument: String,
    },
    SettlementFailed {
        user_id: String,
        position_id: String,
        instrument: String,
        reason: PublicFailureReason,
    },
    PayoutCredited {
        user_id: String,
        position_id: String,
        instrument: String,
        amount: String,
        asset: String,
    },
    MarginCall {
        user_id: String,
        call_id: String,
        shortfall: String,
        asset: String,
        deadline: String,
    },
    LiquidationWarning {
        user_id: String,
        position_id: String,
        instrument: String,
        deadline: String,
    },
    RolloverExecuted {
        user_id: String,
        position_id: String,
        instrument: String,
        rolled_into: String,
    },
}

impl LifecycleSignal {
    /// Map a signal to the notification intent, including the business
    /// entity used for deduplication.
    pub fn to_event(&self) -> NotificationEvent {
        let pos = |id: &str| format!("position:{id}");
        let with = |instrument: &str| TemplateData {
            instrument: Some(instrument.to_string()),
            ..Default::default()
        };
        let (user_id, event, entity_id, data) = match self {
            LifecycleSignal::ExpiryApproaching {
                user_id,
                position_id,
                instrument,
                hours_before,
            } => (
                user_id,
                EventType::ExpiryReminder {
                    hours_before: *hours_before,
                },
                pos(position_id),
                with(instrument),
            ),
            LifecycleSignal::SettlementStarted {
                user_id,
                position_id,
                instrument,
            } => (
                user_id,
                EventType::SettlementStarted,
                pos(position_id),
                with(instrument),
            ),
            LifecycleSignal::SettlementCompleted {
                user_id,
                position_id,
                instrument,
            } => (
                user_id,
                EventType::SettlementCompleted,
                pos(position_id),
                with(instrument),
            ),
            LifecycleSignal::SettlementFailed {
                user_id,
                position_id,
                instrument,
                reason,
            } => (
                user_id,
                EventType::SettlementFailed,
                pos(position_id),
                TemplateData {
                    failure_reason: Some(*reason),
                    ..with(instrument)
                },
            ),
            LifecycleSignal::PayoutCredited {
                user_id,
                position_id,
                instrument,
                amount,
                asset,
            } => (
                user_id,
                EventType::PayoutCredited,
                pos(position_id),
                TemplateData {
                    amount: Some(amount.clone()),
                    asset: Some(asset.clone()),
                    ..with(instrument)
                },
            ),
            LifecycleSignal::MarginCall {
                user_id,
                call_id,
                shortfall,
                asset,
                deadline,
            } => (
                user_id,
                EventType::MarginCall,
                format!("margin_call:{call_id}"),
                TemplateData {
                    amount: Some(shortfall.clone()),
                    asset: Some(asset.clone()),
                    deadline: Some(deadline.clone()),
                    ..Default::default()
                },
            ),
            LifecycleSignal::LiquidationWarning {
                user_id,
                position_id,
                instrument,
                deadline,
            } => (
                user_id,
                EventType::LiquidationWarning,
                pos(position_id),
                TemplateData {
                    deadline: Some(deadline.clone()),
                    ..with(instrument)
                },
            ),
            LifecycleSignal::RolloverExecuted {
                user_id,
                position_id,
                instrument,
                rolled_into,
            } => (
                user_id,
                EventType::RolloverExecuted,
                pos(position_id),
                TemplateData {
                    rolled_into: Some(rolled_into.clone()),
                    ..with(instrument)
                },
            ),
        };
        NotificationEvent {
            user_id: user_id.clone(),
            event,
            entity_id,
            data,
        }
    }
}

/// Settlement event stream hook: record the notification intents for
/// `signal` inside the caller's transaction.
pub async fn notify(
    conn: &mut sqlx::SqliteConnection,
    signal: &LifecycleSignal,
    cap: RateCap,
    now_ms: i64,
) -> Result<EnqueueReport, sqlx::Error> {
    enqueue(conn, &signal.to_event(), cap, now_ms).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_map_to_catalogue_with_stable_entities() {
        let s = LifecycleSignal::MarginCall {
            user_id: "G1".into(),
            call_id: "42".into(),
            shortfall: "50".into(),
            asset: "USDC".into(),
            deadline: "2026-10-22 08:00 UTC".into(),
        };
        let e = s.to_event();
        assert_eq!(e.event, EventType::MarginCall);
        assert_eq!(e.entity_id, "margin_call:42");

        let r = LifecycleSignal::ExpiryApproaching {
            user_id: "G1".into(),
            position_id: "p1".into(),
            instrument: "XLM-C".into(),
            hours_before: 1,
        }
        .to_event();
        assert_eq!(r.event.key(), "expiry_reminder_1h");
        assert_eq!(r.entity_id, "position:p1");
    }

    #[tokio::test]
    async fn notify_writes_intents_in_callers_transaction() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/0012_settlement_notifications.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let signal = LifecycleSignal::SettlementFailed {
            user_id: "G1".into(),
            position_id: "p1".into(),
            instrument: "XLM-C".into(),
            reason: PublicFailureReason::Retrying,
        };
        let r = notify(&mut tx, &signal, RateCap::default(), 0)
            .await
            .unwrap();
        assert_eq!(r.queued, Channel::ALL.to_vec());
        tx.commit().await.unwrap();
    }
}
