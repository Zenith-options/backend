//! Channel providers. Each transport is a [`ChannelProvider`] so email and
//! push backends are swappable and the dispatcher can be tested with
//! scripted providers.

use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;

use super::catalogue::{Channel, Severity};

/// What a provider is asked to send. Contains only rendered, sanitised text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutboundMessage {
    /// Outbox row id; providers that support it should pass it on as an
    /// idempotency key so a retried send is not shown twice.
    pub id: String,
    pub user_id: String,
    pub event_type: String,
    pub severity: Severity,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// Retry with backoff (timeouts, 5xx, 429).
    Transient(String),
    /// Retrying will not help (4xx, no address on file). Goes to `dead`.
    Permanent(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Transient(e) => write!(f, "transient: {e}"),
            SendError::Permanent(e) => write!(f, "permanent: {e}"),
        }
    }
}

#[async_trait]
pub trait ChannelProvider: Send + Sync {
    fn channel(&self) -> Channel;
    async fn send(&self, msg: &OutboundMessage) -> Result<(), SendError>;
}

/// In-app inbox. Delivery means "the row is visible in the user's inbox";
/// the outbox row itself is the inbox record, so this provider only needs
/// to accept the message. It keeps an in-memory feed for live push to
/// connected clients (e.g. a websocket fan-out can drain it).
#[derive(Default)]
pub struct InAppInbox {
    feed: Mutex<Vec<OutboundMessage>>,
}

impl InAppInbox {
    pub fn drain(&self) -> Vec<OutboundMessage> {
        std::mem::take(&mut *self.feed.lock().expect("inbox lock"))
    }
}

#[async_trait]
impl ChannelProvider for InAppInbox {
    fn channel(&self) -> Channel {
        Channel::InApp
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<(), SendError> {
        self.feed.lock().expect("inbox lock").push(msg.clone());
        Ok(())
    }
}

/// Resolves a user id to a channel address (email address, push token).
/// Kept separate so addresses are looked up at send time and never stored
/// in the outbox.
#[async_trait]
pub trait AddressBook: Send + Sync {
    async fn address(&self, user_id: &str, channel: Channel) -> Option<String>;
}

fn classify_status(status: reqwest::StatusCode) -> Result<(), SendError> {
    if status.is_success() {
        Ok(())
    } else if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        Err(SendError::Transient(format!("provider returned {status}")))
    } else {
        Err(SendError::Permanent(format!("provider returned {status}")))
    }
}

fn classify_transport(err: reqwest::Error) -> SendError {
    // Never forward the raw error: it can embed the provider URL.
    if err.is_timeout() {
        SendError::Transient("provider timed out".into())
    } else {
        SendError::Transient("provider unreachable".into())
    }
}

/// Transactional email over a JSON HTTP API
/// (`DELIVERY_EMAIL_API_URL` / `DELIVERY_EMAIL_API_TOKEN` / `DELIVERY_EMAIL_FROM`).
pub struct HttpEmailProvider<A> {
    pub client: reqwest::Client,
    pub api_url: String,
    pub api_token: secrecy::SecretString,
    pub from: String,
    pub addresses: A,
}

#[async_trait]
impl<A: AddressBook> ChannelProvider for HttpEmailProvider<A> {
    fn channel(&self) -> Channel {
        Channel::Email
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<(), SendError> {
        use secrecy::ExposeSecret;
        let to = self
            .addresses
            .address(&msg.user_id, Channel::Email)
            .await
            .ok_or_else(|| SendError::Permanent("no email address on file".into()))?;
        let resp = self
            .client
            .post(&self.api_url)
            .bearer_auth(self.api_token.expose_secret())
            .header("Idempotency-Key", &msg.id)
            .json(&serde_json::json!({
                "from": self.from,
                "to": to,
                "subject": msg.title,
                "text": msg.body,
            }))
            .send()
            .await
            .map_err(classify_transport)?;
        classify_status(resp.status())
    }
}

/// Push notifications through a push gateway (FCM/APNs relay) that accepts
/// `{token, title, body, collapse_key}`.
pub struct HttpPushProvider<A> {
    pub client: reqwest::Client,
    pub gateway_url: String,
    pub api_token: secrecy::SecretString,
    pub addresses: A,
}

#[async_trait]
impl<A: AddressBook> ChannelProvider for HttpPushProvider<A> {
    fn channel(&self) -> Channel {
        Channel::Push
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<(), SendError> {
        use secrecy::ExposeSecret;
        let token = self
            .addresses
            .address(&msg.user_id, Channel::Push)
            .await
            .ok_or_else(|| SendError::Permanent("no push token registered".into()))?;
        let resp = self
            .client
            .post(&self.gateway_url)
            .bearer_auth(self.api_token.expose_secret())
            .json(&serde_json::json!({
                "token": token,
                "title": msg.title,
                "body": msg.body,
                "collapse_key": msg.id,
            }))
            .send()
            .await
            .map_err(classify_transport)?;
        classify_status(resp.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_classification() {
        assert!(classify_status(reqwest::StatusCode::OK).is_ok());
        assert!(matches!(
            classify_status(reqwest::StatusCode::BAD_GATEWAY),
            Err(SendError::Transient(_))
        ));
        assert!(matches!(
            classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            Err(SendError::Transient(_))
        ));
        assert!(matches!(
            classify_status(reqwest::StatusCode::BAD_REQUEST),
            Err(SendError::Permanent(_))
        ));
    }

    #[tokio::test]
    async fn in_app_inbox_feeds_live_clients() {
        let inbox = InAppInbox::default();
        let msg = OutboundMessage {
            id: "n1".into(),
            user_id: "u1".into(),
            event_type: "payout_credited".into(),
            severity: Severity::Info,
            title: "t".into(),
            body: "b".into(),
        };
        inbox.send(&msg).await.unwrap();
        assert_eq!(inbox.drain(), vec![msg]);
        assert!(inbox.drain().is_empty());
    }
}
