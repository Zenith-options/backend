//! Outbound webhooks for settlement lifecycle events.
//!
//! Consumers subscribe an HTTPS endpoint to some or all of the settlement
//! lifecycle events — `settlement.submitted`, `.confirmed`, `.failed`,
//! `.expired` and `.liquidated` — and receive a signed JSON POST for each.
//!
//! **Durable.** [`publish`] writes the event and one delivery row per
//! matching subscription in a single transaction (ideally the same one as the
//! state change it describes). Deliveries survive restarts and are retried
//! with exponential backoff until they succeed.
//!
//! **Ordered per subscription.** Each subscription numbers its deliveries
//! with a gap-free `seq`. The dispatcher only ever sends a subscription's
//! lowest undelivered `seq`, and holds a short lease on the subscription
//! while it does, so two dispatchers can't race. If the head delivery keeps
//! failing, later events wait behind it. When it runs out of attempts the
//! delivery is marked `dead` and the subscription is **suspended**, not
//! skipped past, so a consumer never sees event N+1 without N. An operator
//! calls [`WebhookService::resume`] once the endpoint is fixed; events
//! published in the meantime are queued and delivered in order. Endpoints
//! that answer `410 Gone` are disabled.
//!
//! **Signed.** Each request carries
//! `Zenith-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`
//! (the same `timestamp.payload` scheme as `delivery::sign_payload`), plus
//! `Zenith-Event-Id`, `Zenith-Event-Type` and `Zenith-Delivery-Sequence`.
//! Consumers check it with [`verify_signature`], which also rejects stale
//! timestamps to block replays, and dedupe on the event id.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::sync::Arc;

type HmacSha256 = Hmac<Sha256>;

pub const SIGNATURE_HEADER: &str = "Zenith-Signature";
pub const EVENT_ID_HEADER: &str = "Zenith-Event-Id";
pub const EVENT_TYPE_HEADER: &str = "Zenith-Event-Type";
pub const SEQUENCE_HEADER: &str = "Zenith-Delivery-Sequence";

// ─── Events ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SettlementEventType {
    #[serde(rename = "settlement.submitted")]
    Submitted,
    #[serde(rename = "settlement.confirmed")]
    Confirmed,
    #[serde(rename = "settlement.failed")]
    Failed,
    #[serde(rename = "settlement.expired")]
    Expired,
    #[serde(rename = "settlement.liquidated")]
    Liquidated,
}

impl SettlementEventType {
    pub const ALL: [SettlementEventType; 5] = [
        SettlementEventType::Submitted,
        SettlementEventType::Confirmed,
        SettlementEventType::Failed,
        SettlementEventType::Expired,
        SettlementEventType::Liquidated,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SettlementEventType::Submitted => "settlement.submitted",
            SettlementEventType::Confirmed => "settlement.confirmed",
            SettlementEventType::Failed => "settlement.failed",
            SettlementEventType::Expired => "settlement.expired",
            SettlementEventType::Liquidated => "settlement.liquidated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// A settlement lifecycle event to publish.
#[derive(Debug, Clone)]
pub struct SettlementEvent {
    pub event_type: SettlementEventType,
    pub settlement_id: String,
    pub data: serde_json::Value,
}

impl SettlementEvent {
    /// A transition happens once per settlement, so this is the natural
    /// dedupe key. Override with [`publish_with_key`] for repeatable events.
    fn default_dedupe_key(&self) -> String {
        format!("{}:{}", self.settlement_id, self.event_type.as_str())
    }
}

/// The JSON body POSTed to consumers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookPayload {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: SettlementEventType,
    pub sequence: i64,
    pub settlement_id: String,
    pub created_at_ms: i64,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub event_id: String,
    /// False if an event with the same dedupe key already existed.
    pub newly_published: bool,
    pub deliveries: usize,
}

/// Records an event and queues it for every matching subscription. Takes a
/// connection so it can share the transaction of the state change it
/// describes; call inside a transaction so fan-out is atomic.
pub async fn publish(
    conn: &mut SqliteConnection,
    event: &SettlementEvent,
    now_ms: i64,
) -> Result<Published, sqlx::Error> {
    publish_with_key(conn, event, &event.default_dedupe_key(), now_ms).await
}

pub async fn publish_with_key(
    conn: &mut SqliteConnection,
    event: &SettlementEvent,
    dedupe_key: &str,
    now_ms: i64,
) -> Result<Published, sqlx::Error> {
    let event_id = uuid::Uuid::new_v4().to_string();
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO settlement_events
             (id, event_type, settlement_id, payload, dedupe_key, created_at_ms)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&event_id)
    .bind(event.event_type.as_str())
    .bind(&event.settlement_id)
    .bind(event.data.to_string())
    .bind(dedupe_key)
    .bind(now_ms)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    if !inserted {
        let existing: String =
            sqlx::query_scalar("SELECT id FROM settlement_events WHERE dedupe_key = ?")
                .bind(dedupe_key)
                .fetch_one(&mut *conn)
                .await?;
        return Ok(Published {
            event_id: existing,
            newly_published: false,
            deliveries: 0,
        });
    }

    // Suspended subscriptions still get the event queued so nothing is lost
    // and order is kept when they resume. Disabled ones don't.
    let subs: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, event_types FROM settlement_webhook_subscriptions
          WHERE status IN ('active', 'suspended')",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut deliveries = 0;
    for (sub_id, types) in subs {
        if !types.split(',').any(|t| t == event.event_type.as_str()) {
            continue;
        }
        let seq: i64 = sqlx::query_scalar(
            "UPDATE settlement_webhook_subscriptions SET next_seq = next_seq + 1
              WHERE id = ? RETURNING next_seq - 1",
        )
        .bind(&sub_id)
        .fetch_one(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO settlement_webhook_deliveries
                 (id, subscription_id, seq, event_id, next_attempt_at_ms, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&sub_id)
        .bind(seq)
        .bind(&event_id)
        .bind(now_ms)
        .bind(now_ms)
        .execute(&mut *conn)
        .await?;
        deliveries += 1;
    }
    Ok(Published {
        event_id,
        newly_published: true,
        deliveries,
    })
}

// ─── Signing ─────────────────────────────────────────────────────────────────

/// `t=<ts>,v1=<hex HMAC-SHA256(secret, "<ts>.<body>")>`.
pub fn sign(secret: &[u8], timestamp_s: i64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(timestamp_s.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let sig = data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes());
    format!("t={timestamp_s},v1={sig}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    Malformed,
    Stale,
    Mismatch,
}

/// Checks a `Zenith-Signature` header in constant time and rejects
/// timestamps more than `tolerance_s` away from `now_s`.
pub fn verify_signature(
    secret: &[u8],
    header: &str,
    body: &[u8],
    now_s: i64,
    tolerance_s: i64,
) -> Result<(), SignatureError> {
    let mut ts = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => ts = v.parse::<i64>().ok(),
            Some(("v1", v)) => sigs.push(v),
            _ => {}
        }
    }
    let ts = ts.ok_or(SignatureError::Malformed)?;
    if sigs.is_empty() {
        return Err(SignatureError::Malformed);
    }
    if (now_s - ts).abs() > tolerance_s {
        return Err(SignatureError::Stale);
    }
    for sig in sigs {
        let Ok(bytes) = data_encoding::HEXLOWER_PERMISSIVE.decode(sig.as_bytes()) else {
            continue;
        };
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        if mac.verify_slice(&bytes).is_ok() {
            return Ok(());
        }
    }
    Err(SignatureError::Mismatch)
}

// ─── Transport ───────────────────────────────────────────────────────────────

/// Sends one HTTP POST. `Ok(status)` for any HTTP response, `Err` for
/// network errors and timeouts.
#[async_trait]
pub trait WebhookTransport: Send + Sync {
    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<u16, String>;
}

/// Production transport. Redirects are not followed, so a subscription
/// can't be bounced to an address it wasn't validated for.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(timeout: std::time::Duration) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client })
    }
}

#[async_trait]
impl WebhookTransport for ReqwestTransport {
    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<u16, String> {
        let mut req = self.client.post(url).body(body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.send()
            .await
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string())
    }
}

// ─── Service ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub max_attempts: i64,
    pub base_backoff_ms: i64,
    pub max_backoff_ms: i64,
    /// How long a dispatcher holds a subscription.
    pub lease_ms: i64,
    /// Max deliveries sent to one subscription per dispatch pass.
    pub max_batch_per_subscription: usize,
    /// Allow `http://` endpoints (local development and tests only).
    pub allow_insecure_http: bool,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            max_attempts: 12,
            base_backoff_ms: 1_000,
            max_backoff_ms: 60 * 60_000,
            lease_ms: 30_000,
            max_batch_per_subscription: 20,
            allow_insecure_http: false,
        }
    }
}

impl WebhookConfig {
    fn backoff_ms(&self, attempts: i64) -> i64 {
        let exp = (attempts.max(1) - 1).min(30) as u32;
        self.base_backoff_ms
            .saturating_mul(1i64 << exp)
            .min(self.max_backoff_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionError {
    InvalidUrl(String),
    NoEventTypes,
    Db(String),
}

impl std::fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriptionError::InvalidUrl(e) => write!(f, "invalid webhook url: {e}"),
            SubscriptionError::NoEventTypes => write!(f, "at least one event type is required"),
            SubscriptionError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for SubscriptionError {}

impl From<sqlx::Error> for SubscriptionError {
    fn from(e: sqlx::Error) -> Self {
        SubscriptionError::Db(e.to_string())
    }
}

/// Returned once at creation; the secret is not shown again.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedSubscription {
    pub id: String,
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Subscription {
    pub id: String,
    pub url: String,
    pub event_types: String,
    pub status: String,
    pub status_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DispatchReport {
    pub delivered: usize,
    pub retries_scheduled: usize,
    pub dead: usize,
    pub subscriptions_suspended: usize,
    pub subscriptions_disabled: usize,
}

enum Attempt {
    Delivered,
    Retry,
    Dead,
    Gone,
}

pub struct WebhookService {
    pool: SqlitePool,
    transport: Arc<dyn WebhookTransport>,
    config: WebhookConfig,
    worker_id: String,
}

impl WebhookService {
    pub fn new(
        pool: SqlitePool,
        transport: Arc<dyn WebhookTransport>,
        config: WebhookConfig,
    ) -> Self {
        Self {
            pool,
            transport,
            config,
            worker_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub async fn subscribe(
        &self,
        url: &str,
        event_types: &[SettlementEventType],
        now_ms: i64,
    ) -> Result<CreatedSubscription, SubscriptionError> {
        let parsed =
            reqwest::Url::parse(url).map_err(|e| SubscriptionError::InvalidUrl(e.to_string()))?;
        match parsed.scheme() {
            "https" => {}
            "http" if self.config.allow_insecure_http => {}
            other => {
                return Err(SubscriptionError::InvalidUrl(format!(
                    "scheme {other} not allowed"
                )))
            }
        }
        if parsed.host_str().is_none() {
            return Err(SubscriptionError::InvalidUrl("missing host".into()));
        }
        if event_types.is_empty() {
            return Err(SubscriptionError::NoEventTypes);
        }
        let mut types: Vec<&str> = event_types.iter().map(|t| t.as_str()).collect();
        types.sort_unstable();
        types.dedup();

        let id = uuid::Uuid::new_v4().to_string();
        let secret = {
            let bytes: [u8; 32] = rand::random();
            format!("whsec_{}", data_encoding::HEXLOWER.encode(&bytes))
        };
        sqlx::query(
            "INSERT INTO settlement_webhook_subscriptions
                 (id, url, secret, event_types, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        )
        .bind(&id)
        .bind(parsed.as_str())
        .bind(&secret)
        .bind(types.join(","))
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(CreatedSubscription { id, secret })
    }

    pub async fn get(&self, id: &str) -> Result<Option<Subscription>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, url, event_types, status, status_reason
               FROM settlement_webhook_subscriptions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Stops deliveries for good. Pending deliveries are kept for audit.
    pub async fn disable(&self, id: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        self.set_status(id, "disabled", "disabled by operator", now_ms)
            .await
    }

    /// Reactivates a suspended subscription and gives its dead deliveries a
    /// fresh attempt budget; they go out first, in order.
    pub async fn resume(&self, id: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE settlement_webhook_subscriptions
                SET status = 'active', status_reason = NULL, updated_at_ms = ?
              WHERE id = ? AND status = 'suspended'",
        )
        .bind(now_ms)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 1 {
            sqlx::query(
                "UPDATE settlement_webhook_deliveries
                    SET status = 'pending', attempts = 0, next_attempt_at_ms = ?
                  WHERE subscription_id = ? AND status = 'dead'",
            )
            .bind(now_ms)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(n == 1)
    }

    /// One dispatch pass over every active subscription with due work.
    pub async fn dispatch_due(&self, now_ms: i64) -> Result<DispatchReport, sqlx::Error> {
        let mut report = DispatchReport::default();
        let subs: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT s.id FROM settlement_webhook_subscriptions s
               JOIN settlement_webhook_deliveries d ON d.subscription_id = s.id
              WHERE s.status = 'active' AND d.status = 'pending' AND d.next_attempt_at_ms <= ?",
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await?;
        for sub in subs {
            if !self.acquire_lease(&sub, now_ms).await? {
                continue;
            }
            let result = self.drain_subscription(&sub, now_ms, &mut report).await;
            self.release_lease(&sub).await?;
            result?;
        }
        Ok(report)
    }

    /// Sends this subscription's due deliveries strictly in `seq` order,
    /// stopping at the first one that doesn't succeed.
    async fn drain_subscription(
        &self,
        sub_id: &str,
        now_ms: i64,
        report: &mut DispatchReport,
    ) -> Result<(), sqlx::Error> {
        for _ in 0..self.config.max_batch_per_subscription {
            let head = sqlx::query(
                "SELECT d.id, d.seq, d.attempts, d.next_attempt_at_ms, d.status AS delivery_status,
                        e.id AS event_id, e.event_type, e.settlement_id, e.payload, e.created_at_ms,
                        s.url, s.secret, s.status
                   FROM settlement_webhook_deliveries d
                   JOIN settlement_events e ON e.id = d.event_id
                   JOIN settlement_webhook_subscriptions s ON s.id = d.subscription_id
                  WHERE d.subscription_id = ? AND d.status IN ('pending', 'dead')
                  ORDER BY d.seq LIMIT 1",
            )
            .bind(sub_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(head) = head else { return Ok(()) };
            // A dead head means the subscription is (or should be) suspended:
            // never deliver past it.
            if head.get::<&str, _>("status") != "active"
                || head.get::<&str, _>("delivery_status") == "dead"
                || head.get::<i64, _>("next_attempt_at_ms") > now_ms
            {
                return Ok(());
            }

            let delivery_id: String = head.get("id");
            let seq: i64 = head.get("seq");
            let attempts: i64 = head.get::<i64, _>("attempts") + 1;
            let event_type = SettlementEventType::parse(head.get("event_type"))
                .expect("event_type constrained by CHECK");
            let payload = WebhookPayload {
                id: head.get("event_id"),
                event_type,
                sequence: seq,
                settlement_id: head.get("settlement_id"),
                created_at_ms: head.get("created_at_ms"),
                data: serde_json::from_str(head.get("payload")).unwrap_or(serde_json::Value::Null),
            };
            let body = serde_json::to_vec(&payload).expect("payload serializes");
            let secret: String = head.get("secret");
            let headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                (
                    SIGNATURE_HEADER.to_string(),
                    sign(secret.as_bytes(), now_ms / 1_000, &body),
                ),
                (EVENT_ID_HEADER.to_string(), payload.id.clone()),
                (
                    EVENT_TYPE_HEADER.to_string(),
                    event_type.as_str().to_string(),
                ),
                (SEQUENCE_HEADER.to_string(), seq.to_string()),
            ];
            let url: String = head.get("url");

            let result = self.transport.post(&url, &headers, body).await;
            let (status_code, error) = match &result {
                Ok(code) => (Some(*code as i64), None),
                Err(e) => (None, Some(e.clone())),
            };
            let outcome = match result {
                Ok(code) if (200..300).contains(&code) => Attempt::Delivered,
                Ok(410) => Attempt::Gone,
                _ if attempts >= self.config.max_attempts => Attempt::Dead,
                _ => Attempt::Retry,
            };

            match outcome {
                Attempt::Delivered => {
                    sqlx::query(
                        "UPDATE settlement_webhook_deliveries
                            SET status = 'delivered', attempts = ?, last_status_code = ?,
                                last_error = NULL, delivered_at_ms = ?
                          WHERE id = ?",
                    )
                    .bind(attempts)
                    .bind(status_code)
                    .bind(now_ms)
                    .bind(&delivery_id)
                    .execute(&self.pool)
                    .await?;
                    report.delivered += 1;
                    continue;
                }
                Attempt::Retry => {
                    sqlx::query(
                        "UPDATE settlement_webhook_deliveries
                            SET attempts = ?, last_status_code = ?, last_error = ?, next_attempt_at_ms = ?
                          WHERE id = ?",
                    )
                    .bind(attempts)
                    .bind(status_code)
                    .bind(&error)
                    .bind(now_ms + self.config.backoff_ms(attempts))
                    .bind(&delivery_id)
                    .execute(&self.pool)
                    .await?;
                    report.retries_scheduled += 1;
                }
                Attempt::Dead => {
                    sqlx::query(
                        "UPDATE settlement_webhook_deliveries
                            SET status = 'dead', attempts = ?, last_status_code = ?, last_error = ?
                          WHERE id = ?",
                    )
                    .bind(attempts)
                    .bind(status_code)
                    .bind(&error)
                    .bind(&delivery_id)
                    .execute(&self.pool)
                    .await?;
                    let reason = format!("delivery seq {seq} failed {attempts} times");
                    self.set_status(sub_id, "suspended", &reason, now_ms)
                        .await?;
                    tracing::warn!(subscription = sub_id, seq, "webhook subscription suspended");
                    report.dead += 1;
                    report.subscriptions_suspended += 1;
                }
                Attempt::Gone => {
                    sqlx::query(
                        "UPDATE settlement_webhook_deliveries
                            SET attempts = ?, last_status_code = 410 WHERE id = ?",
                    )
                    .bind(attempts)
                    .bind(&delivery_id)
                    .execute(&self.pool)
                    .await?;
                    self.set_status(sub_id, "disabled", "endpoint returned 410 Gone", now_ms)
                        .await?;
                    report.subscriptions_disabled += 1;
                }
            }
            return Ok(());
        }
        Ok(())
    }

    async fn acquire_lease(&self, sub_id: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE settlement_webhook_subscriptions SET lease_owner = ?, lease_until_ms = ?
              WHERE id = ? AND (lease_until_ms IS NULL OR lease_until_ms <= ? OR lease_owner = ?)",
        )
        .bind(&self.worker_id)
        .bind(now_ms + self.config.lease_ms)
        .bind(sub_id)
        .bind(now_ms)
        .bind(&self.worker_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    async fn release_lease(&self, sub_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE settlement_webhook_subscriptions SET lease_owner = NULL, lease_until_ms = NULL
              WHERE id = ? AND lease_owner = ?",
        )
        .bind(sub_id)
        .bind(&self.worker_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_status(
        &self,
        id: &str,
        status: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE settlement_webhook_subscriptions
                SET status = ?, status_reason = ?, updated_at_ms = ?
              WHERE id = ? AND status != 'disabled'",
        )
        .bind(status)
        .bind(reason)
        .bind(now_ms)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }
}

/// Runs [`WebhookService::dispatch_due`] every `interval`.
pub async fn run_dispatch_loop(service: Arc<WebhookService>, interval: std::time::Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Err(e) = service.dispatch_due(now_ms).await {
            tracing::error!(error = %e, "webhook dispatch pass failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use SettlementEventType::*;

    type Request = (String, Vec<(String, String)>, Vec<u8>);

    #[derive(Default)]
    struct FakeTransport {
        /// Scripted responses per URL; default 200.
        responses: Mutex<HashMap<String, VecDeque<Result<u16, String>>>>,
        received: Mutex<Vec<Request>>,
    }

    impl FakeTransport {
        fn script(&self, url: &str, rs: impl IntoIterator<Item = Result<u16, String>>) {
            self.responses
                .lock()
                .unwrap()
                .entry(url.to_string())
                .or_default()
                .extend(rs);
        }
        fn payloads(&self, url: &str) -> Vec<WebhookPayload> {
            self.received
                .lock()
                .unwrap()
                .iter()
                .filter(|(u, _, _)| u == url)
                .map(|(_, _, b)| serde_json::from_slice(b).unwrap())
                .collect()
        }
        fn count(&self) -> usize {
            self.received.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl WebhookTransport for FakeTransport {
        async fn post(
            &self,
            url: &str,
            h: &[(String, String)],
            body: Vec<u8>,
        ) -> Result<u16, String> {
            self.received
                .lock()
                .unwrap()
                .push((url.to_string(), h.to_vec(), body));
            self.responses
                .lock()
                .unwrap()
                .get_mut(url)
                .and_then(|q| q.pop_front())
                .unwrap_or(Ok(200))
        }
    }

    async fn setup(config: WebhookConfig) -> (WebhookService, Arc<FakeTransport>, SqlitePool) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0015_settlement_webhooks.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let t = Arc::new(FakeTransport::default());
        (
            WebhookService::new(pool.clone(), t.clone(), config),
            t,
            pool,
        )
    }

    async fn emit(
        pool: &SqlitePool,
        t: SettlementEventType,
        settlement: &str,
        now: i64,
    ) -> Published {
        let mut tx = pool.begin().await.unwrap();
        let ev = SettlementEvent {
            event_type: t,
            settlement_id: settlement.into(),
            data: json!({ "settlement_id": settlement }),
        };
        let p = publish(&mut tx, &ev, now).await.unwrap();
        tx.commit().await.unwrap();
        p
    }

    const A: &str = "https://a.example/hook";
    const B: &str = "https://b.example/hook";

    #[test]
    fn event_types_round_trip() {
        for t in SettlementEventType::ALL {
            assert_eq!(SettlementEventType::parse(t.as_str()), Some(t));
            assert_eq!(serde_json::to_value(t).unwrap(), json!(t.as_str()));
        }
    }

    #[test]
    fn signatures_verify_and_reject_tampering_and_staleness() {
        let body = br#"{"id":"e1"}"#;
        let header = sign(b"secret", 1_700_000_000, body);
        assert!(header.starts_with("t=1700000000,v1="));
        assert_eq!(
            verify_signature(b"secret", &header, body, 1_700_000_100, 300),
            Ok(())
        );
        assert_eq!(
            verify_signature(b"secret", &header, b"{}", 1_700_000_000, 300),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(b"other", &header, body, 1_700_000_000, 300),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(b"secret", &header, body, 1_700_001_000, 300),
            Err(SignatureError::Stale)
        );
        assert_eq!(
            verify_signature(b"secret", "v1=abc", body, 0, 300),
            Err(SignatureError::Malformed)
        );
    }

    #[test]
    fn signature_matches_hmac_sha256_test_vector() {
        // RFC 4231 test case 2 key/data, framed as "<t>.<body>".
        let header = sign(b"Jefe", 0, b"what do ya want for nothing?");
        let mut mac = HmacSha256::new_from_slice(b"Jefe").unwrap();
        mac.update(b"0.what do ya want for nothing?");
        let expected = data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes());
        assert_eq!(header, format!("t=0,v1={expected}"));
    }

    #[tokio::test]
    async fn subscribe_validates_url_and_event_types() {
        let (svc, _, _) = setup(WebhookConfig::default()).await;
        assert!(matches!(
            svc.subscribe("http://a.example", &[Confirmed], 0).await,
            Err(SubscriptionError::InvalidUrl(_))
        ));
        assert!(matches!(
            svc.subscribe("ftp://a.example", &[Confirmed], 0).await,
            Err(SubscriptionError::InvalidUrl(_))
        ));
        assert!(matches!(
            svc.subscribe("not a url", &[Confirmed], 0).await,
            Err(SubscriptionError::InvalidUrl(_))
        ));
        assert_eq!(
            svc.subscribe(A, &[], 0).await.unwrap_err(),
            SubscriptionError::NoEventTypes
        );
        let created = svc
            .subscribe(A, &[Confirmed, Confirmed, Failed], 0)
            .await
            .unwrap();
        assert!(created.secret.starts_with("whsec_"));
        let sub = svc.get(&created.id).await.unwrap().unwrap();
        assert_eq!(sub.event_types, "settlement.confirmed,settlement.failed");
    }

    #[tokio::test]
    async fn delivers_signed_events_only_to_matching_subscriptions() {
        let (svc, t, pool) = setup(WebhookConfig::default()).await;
        let a = svc
            .subscribe(A, &SettlementEventType::ALL, 0)
            .await
            .unwrap();
        svc.subscribe(B, &[Liquidated], 0).await.unwrap();

        let p = emit(&pool, Submitted, "s1", 0).await;
        assert_eq!(p.deliveries, 1);
        emit(&pool, Liquidated, "s2", 0).await;

        let r = svc.dispatch_due(5_000).await.unwrap();
        assert_eq!(r.delivered, 3);
        assert_eq!(t.payloads(B).len(), 1);

        let (_, headers, body) = t
            .received
            .lock()
            .unwrap()
            .iter()
            .find(|(u, _, _)| u == A)
            .unwrap()
            .clone();
        let get = |k: &str| headers.iter().find(|(h, _)| h == k).unwrap().1.clone();
        assert_eq!(
            verify_signature(a.secret.as_bytes(), &get(SIGNATURE_HEADER), &body, 5, 300),
            Ok(())
        );
        assert_eq!(get(EVENT_TYPE_HEADER), "settlement.submitted");
        assert_eq!(get(SEQUENCE_HEADER), "1");
        let payload: WebhookPayload = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.id, get(EVENT_ID_HEADER));
        assert_eq!(
            (payload.event_type, payload.settlement_id.as_str()),
            (Submitted, "s1")
        );

        // Nothing left to deliver.
        assert_eq!(
            svc.dispatch_due(10_000).await.unwrap(),
            DispatchReport::default()
        );
    }

    #[tokio::test]
    async fn duplicate_publish_is_a_no_op() {
        let (svc, t, pool) = setup(WebhookConfig::default()).await;
        svc.subscribe(A, &SettlementEventType::ALL, 0)
            .await
            .unwrap();
        let first = emit(&pool, Confirmed, "s1", 0).await;
        let again = emit(&pool, Confirmed, "s1", 1).await;
        assert_eq!(
            (again.event_id, again.newly_published, again.deliveries),
            (first.event_id, false, 0)
        );
        svc.dispatch_due(5).await.unwrap();
        assert_eq!(t.count(), 1);
    }

    #[tokio::test]
    async fn failing_head_blocks_later_events_to_keep_order() {
        let (svc, t, pool) = setup(WebhookConfig::default()).await;
        svc.subscribe(A, &SettlementEventType::ALL, 0)
            .await
            .unwrap();
        svc.subscribe(B, &SettlementEventType::ALL, 0)
            .await
            .unwrap();
        t.script(A, [Ok(500), Err("connection reset".into())]);

        emit(&pool, Submitted, "s1", 0).await;
        emit(&pool, Confirmed, "s1", 0).await;
        emit(&pool, Submitted, "s2", 0).await;

        let r = svc.dispatch_due(0).await.unwrap();
        assert_eq!(
            (r.delivered, r.retries_scheduled),
            (3, 1),
            "B unaffected by A failing"
        );
        assert_eq!(
            t.payloads(A).len(),
            1,
            "A's later events wait behind its head"
        );

        svc.dispatch_due(500).await.unwrap(); // still backing off
        assert_eq!(t.payloads(A).len(), 1);
        svc.dispatch_due(1_000).await.unwrap(); // second failure
        svc.dispatch_due(3_000).await.unwrap(); // succeeds, drains the rest

        let seqs: Vec<i64> = t.payloads(A).iter().map(|p| p.sequence).collect();
        assert_eq!(
            seqs,
            [1, 1, 1, 2, 3],
            "retries of 1, then strictly in order"
        );
        let types: Vec<_> = t.payloads(A).iter().skip(2).map(|p| p.event_type).collect();
        assert_eq!(types, [Submitted, Confirmed, Submitted]);
    }

    #[tokio::test]
    async fn exhausted_delivery_suspends_subscription_until_resumed() {
        let config = WebhookConfig {
            max_attempts: 2,
            ..WebhookConfig::default()
        };
        let (svc, t, pool) = setup(config).await;
        let a = svc
            .subscribe(A, &SettlementEventType::ALL, 0)
            .await
            .unwrap();
        t.script(A, [Ok(503), Ok(503)]);

        emit(&pool, Expired, "s1", 0).await;
        svc.dispatch_due(0).await.unwrap();
        let r = svc.dispatch_due(1_000).await.unwrap();
        assert_eq!((r.dead, r.subscriptions_suspended), (1, 1));
        assert_eq!(svc.get(&a.id).await.unwrap().unwrap().status, "suspended");

        // Events published while suspended are queued, not delivered.
        emit(&pool, Liquidated, "s2", 2_000).await;
        svc.dispatch_due(10_000).await.unwrap();
        assert_eq!(t.payloads(A).len(), 2);

        assert!(svc.resume(&a.id, 20_000).await.unwrap());
        assert_eq!(svc.dispatch_due(20_000).await.unwrap().delivered, 2);
        let delivered: Vec<_> = t
            .payloads(A)
            .iter()
            .skip(2)
            .map(|p| (p.sequence, p.event_type))
            .collect();
        assert_eq!(delivered, [(1, Expired), (2, Liquidated)]);
    }

    #[tokio::test]
    async fn gone_endpoint_disables_subscription() {
        let (svc, t, pool) = setup(WebhookConfig::default()).await;
        let a = svc
            .subscribe(A, &SettlementEventType::ALL, 0)
            .await
            .unwrap();
        t.script(A, [Ok(410)]);
        emit(&pool, Failed, "s1", 0).await;
        assert_eq!(svc.dispatch_due(0).await.unwrap().subscriptions_disabled, 1);
        assert_eq!(svc.get(&a.id).await.unwrap().unwrap().status, "disabled");
        assert!(!svc.resume(&a.id, 1).await.unwrap(), "disabled is terminal");
        assert_eq!(emit(&pool, Failed, "s2", 1).await.deliveries, 0);
    }

    #[tokio::test]
    async fn leased_subscription_is_skipped_by_other_dispatchers() {
        let (svc, t, pool) = setup(WebhookConfig::default()).await;
        let a = svc
            .subscribe(A, &SettlementEventType::ALL, 0)
            .await
            .unwrap();
        emit(&pool, Submitted, "s1", 0).await;
        sqlx::query("UPDATE settlement_webhook_subscriptions SET lease_owner = 'other', lease_until_ms = 30000 WHERE id = ?")
            .bind(&a.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(svc.dispatch_due(1_000).await.unwrap().delivered, 0);
        assert_eq!(t.count(), 0);
        // The other dispatcher died; its lease expires.
        assert_eq!(svc.dispatch_due(30_000).await.unwrap().delivered, 1);
    }

    #[tokio::test]
    async fn deliveries_survive_restart() {
        let path = std::env::temp_dir().join(format!("wh-{}.db", uuid::Uuid::new_v4()));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        {
            let pool = SqlitePool::connect(&url).await.unwrap();
            sqlx::raw_sql(include_str!("../migrations/0015_settlement_webhooks.sql"))
                .execute(&pool)
                .await
                .unwrap();
            let svc = WebhookService::new(
                pool.clone(),
                Arc::new(FakeTransport::default()),
                WebhookConfig::default(),
            );
            svc.subscribe(A, &[Confirmed], 0).await.unwrap();
            emit(&pool, Confirmed, "s1", 0).await;
            pool.close().await;
        }
        let pool = SqlitePool::connect(&url).await.unwrap();
        let t = Arc::new(FakeTransport::default());
        let svc = WebhookService::new(pool, t.clone(), WebhookConfig::default());
        assert_eq!(svc.dispatch_due(1).await.unwrap().delivered, 1);
        assert_eq!(t.payloads(A)[0].event_type, Confirmed);
        let _ = std::fs::remove_file(path);
    }
}
