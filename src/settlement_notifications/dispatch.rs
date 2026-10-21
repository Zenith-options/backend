//! Transactional outbox and per-channel delivery worker.
//!
//! * [`enqueue`] runs **inside the business transaction** (settlement,
//!   margin, rollover) and writes one outbox row per channel. It does no I/O
//!   beyond the database, so a slow or broken provider can never roll back
//!   or block the business change.
//! * [`Dispatcher::run_once`] drains each channel concurrently and
//!   independently, with exponential backoff, a per-send timeout and a
//!   per-attempt log. A channel that is hard down only reschedules its own
//!   rows.
//!
//! Deduplication is the `UNIQUE (user_id, event_type, entity_id, channel)`
//! constraint: retries of the business transaction, and the same event
//! arriving from several sources (indexer + keeper), collapse to one row.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sqlx::{Row, SqliteConnection, SqlitePool};

use super::catalogue::{render, Channel, EventType, Severity, TemplateData};
use super::channels::{ChannelProvider, OutboundMessage, SendError};
use super::preferences::{resolve, Decision, QuietHours, SuppressReason, UserPreferences};

const HOUR_MS: i64 = 3_600_000;

/// A notification intent produced by a business event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationEvent {
    pub user_id: String,
    pub event: EventType,
    /// The business entity the event is about, e.g. `position:<uuid>`.
    pub entity_id: String,
    pub data: TemplateData,
}

/// Per-user cap on non-critical external (email/push) notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateCap {
    pub max_per_hour: u32,
}

impl Default for RateCap {
    fn default() -> Self {
        Self { max_per_hour: 10 }
    }
}

/// What [`enqueue`] did, per channel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnqueueReport {
    pub queued: Vec<Channel>,
    pub deferred: Vec<Channel>,
    pub suppressed: Vec<(Channel, SuppressReason)>,
    /// Channels where an identical notification already existed.
    pub duplicates: Vec<Channel>,
}

/// Load preferences, falling back to defaults for users who never set any.
pub async fn load_preferences(
    conn: &mut SqliteConnection,
    user_id: &str,
) -> Result<UserPreferences, sqlx::Error> {
    let row = sqlx::query(
        "SELECT global_mute, quiet_start_minute, quiet_end_minute, utc_offset_minutes, overrides \
         FROM notification_preferences WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(UserPreferences::default());
    };
    let quiet_hours = match (row.get::<Option<i64>, _>(1), row.get::<Option<i64>, _>(2)) {
        (Some(start), Some(end)) => Some(QuietHours {
            start_minute: start as u16,
            end_minute: end as u16,
            utc_offset_minutes: row.get::<i64, _>(3) as i16,
        }),
        _ => None,
    };
    // A corrupt overrides blob degrades to defaults rather than blocking
    // the settlement transaction that is enqueueing.
    let overrides = serde_json::from_str(&row.get::<String, _>(4)).unwrap_or_default();
    Ok(UserPreferences {
        global_mute: row.get::<i64, _>(0) != 0,
        quiet_hours,
        overrides,
    })
}

pub async fn save_preferences(
    conn: &mut SqliteConnection,
    user_id: &str,
    prefs: &UserPreferences,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    let overrides = serde_json::to_string(&prefs.overrides).expect("overrides serialise");
    sqlx::query(
        "INSERT INTO notification_preferences \
             (user_id, global_mute, quiet_start_minute, quiet_end_minute, utc_offset_minutes, overrides, updated_at_ms) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (user_id) DO UPDATE SET \
             global_mute = excluded.global_mute, quiet_start_minute = excluded.quiet_start_minute, \
             quiet_end_minute = excluded.quiet_end_minute, utc_offset_minutes = excluded.utc_offset_minutes, \
             overrides = excluded.overrides, updated_at_ms = excluded.updated_at_ms",
    )
    .bind(user_id)
    .bind(prefs.global_mute as i64)
    .bind(prefs.quiet_hours.map(|q| q.start_minute as i64))
    .bind(prefs.quiet_hours.map(|q| q.end_minute as i64))
    .bind(prefs.quiet_hours.map(|q| q.utc_offset_minutes as i64).unwrap_or(0))
    .bind(overrides)
    .bind(now_ms)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn recent_external_count(
    conn: &mut SqliteConnection,
    user_id: &str,
    channel: Channel,
    now_ms: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM notification_outbox \
         WHERE user_id = ? AND channel = ? AND status != 'suppressed' \
           AND severity != 'critical' AND created_at_ms > ?",
    )
    .bind(user_id)
    .bind(channel.as_str())
    .bind(now_ms - HOUR_MS)
    .fetch_one(&mut *conn)
    .await
}

/// Write notification intents for `event` on every channel. Call this with
/// the business transaction's connection (`&mut *tx`) so the intent commits
/// or rolls back atomically with the state change it describes.
pub async fn enqueue(
    conn: &mut SqliteConnection,
    event: &NotificationEvent,
    cap: RateCap,
    now_ms: i64,
) -> Result<EnqueueReport, sqlx::Error> {
    let prefs = load_preferences(conn, &event.user_id).await?;
    let severity = event.event.severity();
    let message = render(event.event, &event.data);
    let event_key = event.event.key();
    let mut report = EnqueueReport::default();

    for channel in Channel::ALL {
        let mut decision = resolve(&prefs, event.event, channel, now_ms);
        let rate_limited = channel != Channel::InApp && severity != Severity::Critical;
        if rate_limited && !matches!(decision, Decision::Suppress(_)) {
            let sent = recent_external_count(conn, &event.user_id, channel, now_ms).await?;
            if sent >= cap.max_per_hour as i64 {
                decision = Decision::Suppress(SuppressReason::RateCapped);
            }
        }
        let (status, reason, next_attempt) = match decision {
            Decision::Deliver => ("pending", None, now_ms),
            Decision::Defer { until_ms } => ("pending", None, until_ms),
            Decision::Suppress(r) => ("suppressed", Some(r.as_str()), now_ms),
        };
        let inserted = sqlx::query(
            "INSERT INTO notification_outbox \
                 (id, user_id, event_type, entity_id, channel, severity, title, body, status, \
                  suppressed_reason, next_attempt_at_ms, created_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (user_id, event_type, entity_id, channel) DO NOTHING",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&event.user_id)
        .bind(&event_key)
        .bind(&event.entity_id)
        .bind(channel.as_str())
        .bind(severity.as_str())
        .bind(&message.title)
        .bind(&message.body)
        .bind(status)
        .bind(reason)
        .bind(next_attempt)
        .bind(now_ms)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1;

        match (inserted, decision) {
            (false, _) => report.duplicates.push(channel),
            (true, Decision::Deliver) => report.queued.push(channel),
            (true, Decision::Defer { .. }) => report.deferred.push(channel),
            (true, Decision::Suppress(r)) => report.suppressed.push((channel, r)),
        }
    }
    Ok(report)
}

/// Exponential backoff with a cap and a maximum attempt count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub base_delay_ms: i64,
    pub max_delay_ms: i64,
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // 30s, 1m, 2m, 4m ... capped at 1h; give up after 8 attempts (~2h).
        Self {
            base_delay_ms: 30_000,
            max_delay_ms: HOUR_MS,
            max_attempts: 8,
        }
    }
}

impl RetryPolicy {
    /// Delay before attempt `attempt + 1`, given `attempt` attempts made.
    pub fn backoff_ms(&self, attempt: u32) -> i64 {
        let exp = attempt.saturating_sub(1).min(30);
        self.base_delay_ms
            .saturating_mul(1i64 << exp)
            .min(self.max_delay_ms)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ChannelStats {
    pub delivered: u32,
    pub retried: u32,
    pub dead: u32,
}

pub struct Dispatcher {
    pool: SqlitePool,
    providers: Vec<Arc<dyn ChannelProvider>>,
    pub policy: RetryPolicy,
    pub send_timeout: Duration,
    pub batch_size: i64,
}

#[derive(Clone, Copy)]
enum AttemptOutcome {
    Delivered,
    Transient,
    Permanent,
    Timeout,
}

impl AttemptOutcome {
    fn as_str(self) -> &'static str {
        match self {
            AttemptOutcome::Delivered => "delivered",
            AttemptOutcome::Transient => "transient_failure",
            AttemptOutcome::Permanent => "permanent_failure",
            AttemptOutcome::Timeout => "timeout",
        }
    }
}

impl Dispatcher {
    pub fn new(pool: SqlitePool, providers: Vec<Arc<dyn ChannelProvider>>) -> Self {
        Self {
            pool,
            providers,
            policy: RetryPolicy::default(),
            send_timeout: Duration::from_secs(10),
            batch_size: 100,
        }
    }

    /// Drain every configured channel once, concurrently. Channels without a
    /// provider are left pending.
    pub async fn run_once(
        &self,
        now_ms: i64,
    ) -> Result<BTreeMap<Channel, ChannelStats>, sqlx::Error> {
        let mut set = tokio::task::JoinSet::new();
        for provider in &self.providers {
            let (pool, provider) = (self.pool.clone(), provider.clone());
            let (policy, timeout, batch) = (self.policy, self.send_timeout, self.batch_size);
            set.spawn(async move {
                let channel = provider.channel();
                (
                    channel,
                    drain_channel(&pool, provider, policy, timeout, batch, now_ms).await,
                )
            });
        }
        let mut out = BTreeMap::new();
        let mut first_err = None;
        while let Some(joined) = set.join_next().await {
            let (channel, result) = joined.expect("dispatch task panicked");
            match result {
                Ok(stats) => {
                    out.insert(channel, stats);
                }
                Err(e) => {
                    tracing::error!(channel = channel.as_str(), error = %e, "notification channel drain failed");
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) if out.is_empty() => Err(e),
            _ => Ok(out),
        }
    }
}

async fn drain_channel(
    pool: &SqlitePool,
    provider: Arc<dyn ChannelProvider>,
    policy: RetryPolicy,
    timeout: Duration,
    batch: i64,
    now_ms: i64,
) -> Result<ChannelStats, sqlx::Error> {
    let channel = provider.channel();
    let rows = sqlx::query(
        "SELECT id, user_id, event_type, severity, title, body, attempts, next_attempt_at_ms \
         FROM notification_outbox \
         WHERE channel = ? AND status = 'pending' AND next_attempt_at_ms <= ? \
         ORDER BY next_attempt_at_ms LIMIT ?",
    )
    .bind(channel.as_str())
    .bind(now_ms)
    .bind(batch)
    .fetch_all(pool)
    .await?;

    let mut stats = ChannelStats::default();
    let lease_until = now_ms + timeout.as_millis() as i64 * 2;
    for row in rows {
        let id: String = row.get(0);
        let attempts: i64 = row.get(6);
        let seen_next: i64 = row.get(7);

        // Optimistic claim so two workers never send the same row.
        let claimed = sqlx::query(
            "UPDATE notification_outbox SET next_attempt_at_ms = ? \
             WHERE id = ? AND status = 'pending' AND next_attempt_at_ms = ?",
        )
        .bind(lease_until)
        .bind(&id)
        .bind(seen_next)
        .execute(pool)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            continue;
        }

        let severity = match row.get::<String, _>(3).as_str() {
            "critical" => Severity::Critical,
            "warning" => Severity::Warning,
            _ => Severity::Info,
        };
        let msg = OutboundMessage {
            id: id.clone(),
            user_id: row.get(1),
            event_type: row.get(2),
            severity,
            title: row.get(4),
            body: row.get(5),
        };

        let (outcome, error) = match tokio::time::timeout(timeout, provider.send(&msg)).await {
            Ok(Ok(())) => (AttemptOutcome::Delivered, None),
            Ok(Err(SendError::Transient(e))) => (AttemptOutcome::Transient, Some(e)),
            Ok(Err(SendError::Permanent(e))) => (AttemptOutcome::Permanent, Some(e)),
            Err(_) => (AttemptOutcome::Timeout, Some("send timed out".to_string())),
        };
        let attempt = attempts as u32 + 1;

        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO notification_attempts (outbox_id, attempt, attempted_at_ms, outcome, error) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(attempt as i64)
        .bind(now_ms)
        .bind(outcome.as_str())
        .bind(&error)
        .execute(&mut *tx)
        .await?;

        let give_up =
            matches!(outcome, AttemptOutcome::Permanent) || attempt >= policy.max_attempts;
        let (status, next) = match outcome {
            AttemptOutcome::Delivered => ("delivered", now_ms),
            _ if give_up => ("dead", now_ms),
            _ => ("pending", now_ms + policy.backoff_ms(attempt)),
        };
        sqlx::query(
            "UPDATE notification_outbox SET status = ?, attempts = ?, next_attempt_at_ms = ?, last_error = ?, \
                 delivered_at_ms = CASE WHEN ? = 'delivered' THEN ? ELSE delivered_at_ms END \
             WHERE id = ?",
        )
        .bind(status)
        .bind(attempt as i64)
        .bind(next)
        .bind(&error)
        .bind(status)
        .bind(now_ms)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        match status {
            "delivered" => stats.delivered += 1,
            "dead" => {
                stats.dead += 1;
                tracing::warn!(channel = channel.as_str(), outbox_id = %id, "notification dead-lettered");
            }
            _ => stats.retried += 1,
        }
    }
    Ok(stats)
}

/// An in-app inbox entry as returned to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InboxItem {
    pub id: String,
    pub event_type: String,
    pub entity_id: String,
    pub severity: String,
    pub title: String,
    pub body: String,
    pub created_at_ms: i64,
    pub read_at_ms: Option<i64>,
}

/// Newest-first in-app inbox, keyset-paginated on `created_at_ms`.
pub async fn list_inbox(
    pool: &SqlitePool,
    user_id: &str,
    before_ms: Option<i64>,
    limit: i64,
) -> Result<Vec<InboxItem>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, event_type, entity_id, severity, title, body, created_at_ms, read_at_ms \
         FROM notification_outbox \
         WHERE user_id = ? AND channel = 'in_app' AND status != 'suppressed' AND created_at_ms < ? \
         ORDER BY created_at_ms DESC, id DESC LIMIT ?",
    )
    .bind(user_id)
    .bind(before_ms.unwrap_or(i64::MAX))
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| InboxItem {
            id: r.get(0),
            event_type: r.get(1),
            entity_id: r.get(2),
            severity: r.get(3),
            title: r.get(4),
            body: r.get(5),
            created_at_ms: r.get(6),
            read_at_ms: r.get(7),
        })
        .collect())
}

/// Mark an in-app notification read. Scoped to `user_id` so one user can
/// never touch another user's inbox. Returns whether a row changed.
pub async fn mark_read(
    pool: &SqlitePool,
    user_id: &str,
    id: &str,
    now_ms: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE notification_outbox SET read_at_ms = ? \
         WHERE id = ? AND user_id = ? AND channel = 'in_app' AND read_at_ms IS NULL",
    )
    .bind(now_ms)
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::atomic::{AtomicU32, Ordering};

    const NOW: i64 = 1_792_540_800_000;

    pub(crate) async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
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
        pool
    }

    fn payout(entity: &str) -> NotificationEvent {
        NotificationEvent {
            user_id: "GUSER".into(),
            event: EventType::PayoutCredited,
            entity_id: entity.into(),
            data: TemplateData {
                amount: Some("10".into()),
                asset: Some("USDC".into()),
                ..Default::default()
            },
        }
    }

    struct Scripted {
        channel: Channel,
        calls: AtomicU32,
        result: fn(u32) -> Result<(), SendError>,
        hang: bool,
    }

    impl Scripted {
        fn new(channel: Channel, result: fn(u32) -> Result<(), SendError>) -> Arc<Self> {
            Arc::new(Self {
                channel,
                calls: AtomicU32::new(0),
                result,
                hang: false,
            })
        }
    }

    #[async_trait]
    impl ChannelProvider for Scripted {
        fn channel(&self) -> Channel {
            self.channel
        }
        async fn send(&self, _msg: &OutboundMessage) -> Result<(), SendError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.hang {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            (self.result)(n)
        }
    }

    async fn status_of(pool: &SqlitePool, channel: Channel) -> (String, i64) {
        let row = sqlx::query("SELECT status, attempts FROM notification_outbox WHERE channel = ?")
            .bind(channel.as_str())
            .fetch_one(pool)
            .await
            .unwrap();
        (row.get(0), row.get(1))
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let p = RetryPolicy {
            base_delay_ms: 1_000,
            max_delay_ms: 10_000,
            max_attempts: 5,
        };
        assert_eq!(
            [1, 2, 3, 4, 5].map(|a| p.backoff_ms(a)),
            [1_000, 2_000, 4_000, 8_000, 10_000]
        );
        assert_eq!(p.backoff_ms(u32::MAX), 10_000);
    }

    #[tokio::test]
    async fn enqueue_applies_preferences_per_channel() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        let r = enqueue(&mut conn, &payout("position:1"), RateCap::default(), NOW)
            .await
            .unwrap();
        // Payout defaults: in-app, email and push all on.
        assert_eq!(
            r.queued,
            vec![Channel::InApp, Channel::Email, Channel::Push]
        );

        let ev = NotificationEvent {
            event: EventType::SettlementStarted,
            ..payout("position:1")
        };
        let r = enqueue(&mut conn, &ev, RateCap::default(), NOW)
            .await
            .unwrap();
        assert_eq!(r.queued, vec![Channel::InApp]);
        assert_eq!(
            r.suppressed,
            vec![
                (Channel::Email, SuppressReason::DisabledByDefault),
                (Channel::Push, SuppressReason::DisabledByDefault)
            ]
        );
    }

    #[tokio::test]
    async fn dedupes_across_business_retries_and_sources() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        enqueue(&mut conn, &payout("position:1"), RateCap::default(), NOW)
            .await
            .unwrap();
        // Same event re-emitted by a retried settlement batch / a second source.
        let again = enqueue(
            &mut conn,
            &payout("position:1"),
            RateCap::default(),
            NOW + 5,
        )
        .await
        .unwrap();
        assert!(again.queued.is_empty());
        assert_eq!(again.duplicates, Channel::ALL.to_vec());
        drop(conn);

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 3);

        // Dispatch retries do not create rows either.
        let flaky = Scripted::new(Channel::Email, |n| {
            if n == 1 {
                Err(SendError::Transient("503".into()))
            } else {
                Ok(())
            }
        });
        let d = Dispatcher::new(pool.clone(), vec![flaky.clone()]);
        d.run_once(NOW).await.unwrap();
        d.run_once(NOW + HOUR_MS).await.unwrap();
        assert_eq!(
            status_of(&pool, Channel::Email).await,
            ("delivered".to_string(), 2)
        );
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 3);
        let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification_attempts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn rolled_back_business_transaction_leaves_no_intent() {
        let pool = pool().await;
        let mut tx = pool.begin().await.unwrap();
        enqueue(&mut tx, &payout("position:1"), RateCap::default(), NOW)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn retries_with_backoff_then_dead_letters() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        enqueue(&mut conn, &payout("position:1"), RateCap::default(), NOW)
            .await
            .unwrap();
        drop(conn);
        let down = Scripted::new(Channel::Email, |_| Err(SendError::Transient("503".into())));
        let mut d = Dispatcher::new(pool.clone(), vec![down.clone()]);
        d.policy = RetryPolicy {
            base_delay_ms: 1_000,
            max_delay_ms: 60_000,
            max_attempts: 3,
        };

        d.run_once(NOW).await.unwrap();
        // Not yet due: backoff is 1s after the first failure.
        d.run_once(NOW + 500).await.unwrap();
        assert_eq!(down.calls.load(Ordering::SeqCst), 1);
        d.run_once(NOW + 1_000).await.unwrap();
        assert_eq!(
            status_of(&pool, Channel::Email).await,
            ("pending".to_string(), 2)
        );
        let next: i64 = sqlx::query_scalar(
            "SELECT next_attempt_at_ms FROM notification_outbox WHERE channel='email'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(next, NOW + 1_000 + 2_000);
        let stats = d.run_once(next).await.unwrap();
        assert_eq!(stats[&Channel::Email].dead, 1);
        assert_eq!(
            status_of(&pool, Channel::Email).await,
            ("dead".to_string(), 3)
        );
    }

    #[tokio::test]
    async fn permanent_failure_dead_letters_immediately() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        enqueue(&mut conn, &payout("position:1"), RateCap::default(), NOW)
            .await
            .unwrap();
        drop(conn);
        let p = Scripted::new(Channel::Push, |_| {
            Err(SendError::Permanent("no token".into()))
        });
        Dispatcher::new(pool.clone(), vec![p])
            .run_once(NOW)
            .await
            .unwrap();
        assert_eq!(
            status_of(&pool, Channel::Push).await,
            ("dead".to_string(), 1)
        );
    }

    /// Failure injection: email hangs forever and push is hard down; in-app
    /// must still deliver in the same pass, and nothing stalls.
    #[tokio::test]
    async fn hard_down_channel_does_not_stall_others() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        for i in 0..5 {
            enqueue(
                &mut conn,
                &payout(&format!("position:{i}")),
                RateCap::default(),
                NOW,
            )
            .await
            .unwrap();
        }
        drop(conn);
        let hanging = Arc::new(Scripted {
            channel: Channel::Email,
            calls: AtomicU32::new(0),
            result: |_| Ok(()),
            hang: true,
        });
        let down = Scripted::new(Channel::Push, |_| {
            Err(SendError::Transient("connection refused".into()))
        });
        let in_app = Arc::new(super::super::channels::InAppInbox::default());
        let mut d = Dispatcher::new(pool.clone(), vec![hanging, down, in_app.clone()]);
        d.send_timeout = Duration::from_millis(20);

        let stats = tokio::time::timeout(Duration::from_secs(5), d.run_once(NOW))
            .await
            .expect("dispatch stalled")
            .unwrap();
        assert_eq!(stats[&Channel::InApp].delivered, 5);
        assert_eq!(stats[&Channel::Email].retried, 5);
        assert_eq!(stats[&Channel::Push].retried, 5);
        assert_eq!(in_app.drain().len(), 5);
        let timeouts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notification_attempts WHERE outcome = 'timeout'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(timeouts, 5);
    }

    #[tokio::test]
    async fn rate_cap_limits_non_critical_external_only() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        let cap = RateCap { max_per_hour: 2 };
        for i in 0..3 {
            enqueue(&mut conn, &payout(&format!("position:{i}")), cap, NOW + i)
                .await
                .unwrap();
        }
        let r = enqueue(&mut conn, &payout("position:9"), cap, NOW + 10)
            .await
            .unwrap();
        assert_eq!(r.queued, vec![Channel::InApp]);
        assert_eq!(
            r.suppressed,
            vec![
                (Channel::Email, SuppressReason::RateCapped),
                (Channel::Push, SuppressReason::RateCapped)
            ]
        );
        // Critical events are never capped.
        let call = NotificationEvent {
            event: EventType::MarginCall,
            ..payout("margin_call:1")
        };
        let r = enqueue(&mut conn, &call, cap, NOW + 11).await.unwrap();
        assert_eq!(r.queued, Channel::ALL.to_vec());
        // The window slides.
        let r = enqueue(&mut conn, &payout("position:10"), cap, NOW + HOUR_MS + 100)
            .await
            .unwrap();
        assert_eq!(r.queued, Channel::ALL.to_vec());
    }

    #[tokio::test]
    async fn quiet_hours_and_preferences_round_trip() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        let mut prefs = UserPreferences {
            quiet_hours: Some(QuietHours {
                start_minute: 0,
                end_minute: 6 * 60,
                utc_offset_minutes: 0,
            }),
            ..Default::default()
        };
        prefs.set_override(EventType::PayoutCredited, Channel::Push, false);
        save_preferences(&mut conn, "GUSER", &prefs, NOW)
            .await
            .unwrap();
        assert_eq!(load_preferences(&mut conn, "GUSER").await.unwrap(), prefs);

        let r = enqueue(
            &mut conn,
            &payout("position:1"),
            RateCap::default(),
            NOW + 60_000,
        )
        .await
        .unwrap();
        assert_eq!(r.queued, vec![Channel::InApp]);
        assert_eq!(r.deferred, vec![Channel::Email]);
        assert_eq!(
            r.suppressed,
            vec![(Channel::Push, SuppressReason::OptedOut)]
        );
        drop(conn);

        // The deferred email is not sent until quiet hours end.
        let email = Scripted::new(Channel::Email, |_| Ok(()));
        let d = Dispatcher::new(pool.clone(), vec![email.clone()]);
        d.run_once(NOW + HOUR_MS).await.unwrap();
        assert_eq!(email.calls.load(Ordering::SeqCst), 0);
        d.run_once(NOW + 6 * HOUR_MS).await.unwrap();
        assert_eq!(email.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn inbox_lists_and_marks_read_for_owner_only() {
        let pool = pool().await;
        let mut conn = pool.acquire().await.unwrap();
        enqueue(&mut conn, &payout("position:1"), RateCap::default(), NOW)
            .await
            .unwrap();
        let other = NotificationEvent {
            user_id: "GOTHER".into(),
            ..payout("position:2")
        };
        enqueue(&mut conn, &other, RateCap::default(), NOW + 1)
            .await
            .unwrap();
        drop(conn);

        let items = list_inbox(&pool, "GUSER", None, 50).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(!mark_read(&pool, "GOTHER", &items[0].id, NOW).await.unwrap());
        assert!(mark_read(&pool, "GUSER", &items[0].id, NOW).await.unwrap());
        assert_eq!(
            list_inbox(&pool, "GUSER", None, 50).await.unwrap()[0].read_at_ms,
            Some(NOW)
        );
    }
}
