use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::broadcast;
use uuid::Uuid;

const EVENT_CAPACITY: usize = 2048;
const MAX_REPLAY_EVENTS: usize = 1000;
const MAX_MESSAGE_BYTES: usize = 8192;

#[derive(Clone)]
pub struct GatewayState {
    pool: SqlitePool,
    handoff: Arc<dyn SettlementHandoff>,
    max_order_notional: f64,
}

impl GatewayState {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            handoff: Arc::new(UnavailableSettlementHandoff),
            max_order_notional: 0.0,
        }
    }

    /// Configure the actual settlement adapter and its per-order notional cap.
    pub fn with_settlement_handoff(
        mut self,
        handoff: Arc<dyn SettlementHandoff>,
        max_order_notional: f64,
    ) -> Result<Self, GatewayConfigError> {
        if !max_order_notional.is_finite() || max_order_notional <= 0.0 {
            return Err(GatewayConfigError);
        }
        self.handoff = handoff;
        self.max_order_notional = max_order_notional;
        Ok(self)
    }
}

#[derive(Debug)]
pub struct GatewayConfigError;

impl std::fmt::Display for GatewayConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "market-maker order notional limit must be finite and positive")
    }
}

impl std::error::Error for GatewayConfigError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderIntent {
    pub client_order_id: Uuid,
    pub series: String,
    pub side: OrderSide,
    pub quantity: f64,
    pub limit_price: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffReceipt {
    pub settlement_reference: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HandoffFailure;

#[async_trait::async_trait]
pub trait SettlementHandoff: Send + Sync {
    /// Implementations must be idempotent for a given correlation ID.
    async fn submit(
        &self,
        wallet: &str,
        correlation_id: Uuid,
        intent: &OrderIntent,
    ) -> Result<HandoffReceipt, HandoffFailure>;
}

struct UnavailableSettlementHandoff;

#[async_trait::async_trait]
impl SettlementHandoff for UnavailableSettlementHandoff {
    async fn submit(
        &self,
        _wallet: &str,
        _correlation_id: Uuid,
        _intent: &OrderIntent,
    ) -> Result<HandoffReceipt, HandoffFailure> {
        Err(HandoffFailure)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayEvent {
    pub sequence: i64,
    pub kind: String,
    pub correlation_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
struct StoredEvent {
    event: GatewayEvent,
    audience_wallet: Option<String>,
}

static EVENT_BUS: OnceLock<broadcast::Sender<StoredEvent>> = OnceLock::new();

fn event_bus() -> &'static broadcast::Sender<StoredEvent> {
    EVENT_BUS.get_or_init(|| broadcast::channel(EVENT_CAPACITY).0)
}

/// Persist and publish a series, oracle, or margin event for gateway clients.
/// Private margin events should set `audience_wallet`; market-wide updates
/// should leave it unset.
pub async fn publish_event(
    pool: &SqlitePool,
    kind: &str,
    correlation_id: Option<&str>,
    audience_wallet: Option<&str>,
    payload: &Value,
) -> Result<i64, sqlx::Error> {
    let serialized_payload = serde_json::to_string(payload)
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let sequence = sqlx::query_scalar::<_, i64>(
        "INSERT INTO market_maker_events (kind, correlation_id, audience_wallet, payload) \
         VALUES (?, ?, ?, ?) RETURNING sequence",
    )
    .bind(kind)
    .bind(correlation_id)
    .bind(audience_wallet)
    .bind(serialized_payload)
    .fetch_one(pool)
    .await?;

    let _ = event_bus().send(StoredEvent {
        event: GatewayEvent {
            sequence,
            kind: kind.to_string(),
            correlation_id: correlation_id.map(str::to_string),
            payload: payload.clone(),
        },
        audience_wallet: audience_wallet.map(str::to_string),
    });
    Ok(sequence)
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    #[serde(default)]
    after: i64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Quote {
        series: String,
        sequence: i64,
        bid: f64,
        ask: f64,
    },
    OrderIntent(OrderIntent),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ready { wallet: String, sequence: i64 },
    Event(GatewayEvent),
    QuoteAccepted { series: String, sequence: i64 },
    TradeAccepted {
        client_order_id: Uuid,
        correlation_id: Uuid,
        settlement_reference: String,
    },
    TradeRejected {
        client_order_id: Uuid,
        correlation_id: Uuid,
        reason: String,
    },
    Error { code: String, message: String },
    ResyncRequired,
}

pub fn routes(state: GatewayState) -> Router {
    Router::new()
        .route("/api/v1/market-maker/stream", get(connect))
        .with_state(state)
}

async fn connect(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if query.after < 0 {
        return api_error(StatusCode::BAD_REQUEST, "invalid_cursor", "after must be non-negative");
    }

    let Some(token) = bearer_token(&headers) else {
        return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "valid bearer token required");
    };
    let wallet = match authenticated_market_maker(&state.pool, token).await {
        Ok(Some(wallet)) => wallet,
        Ok(None) => {
            return api_error(
                StatusCode::FORBIDDEN,
                "market_maker_access_required",
                "an active market-maker account is required",
            )
        }
        Err(error) => {
            tracing::error!(%error, "market-maker authentication lookup failed");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication_unavailable",
                "market-maker authentication is temporarily unavailable",
            );
        }
    };

    let token = token.to_string();
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve(socket, state, wallet, token, query.after))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
}

async fn authenticated_market_maker(
    pool: &SqlitePool,
    token: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT s.wallet_address FROM sessions s \
         INNER JOIN market_maker_accounts m ON m.wallet_address = s.wallet_address \
         WHERE s.token = ? \
           AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
           AND m.enabled = 1",
    )
    .bind(token)
    .fetch_optional(pool)
    .await
}

async fn serve(
    mut socket: WebSocket,
    state: GatewayState,
    wallet: String,
    token: String,
    after: i64,
) {
    let mut events = event_bus().subscribe();
    let replay = match sqlx::query_as::<_, EventRow>(
        "SELECT sequence, kind, correlation_id, payload \
         FROM market_maker_events \
         WHERE sequence > ? AND (audience_wallet IS NULL OR audience_wallet = ?) \
         ORDER BY sequence LIMIT ?",
    )
    .bind(after)
    .bind(&wallet)
    .bind((MAX_REPLAY_EVENTS + 1) as i64)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(%error, "market-maker event replay failed");
            let _ = send_message(
                &mut socket,
                &ServerMessage::Error {
                    code: "event_replay_unavailable".into(),
                    message: "event replay is temporarily unavailable".into(),
                },
            )
            .await;
            return;
        }
    };

    if replay.len() > MAX_REPLAY_EVENTS {
        let _ = send_message(&mut socket, &ServerMessage::ResyncRequired).await;
        return;
    }

    let mut cursor = after;
    if send_message(
        &mut socket,
        &ServerMessage::Ready {
            wallet: wallet.clone(),
            sequence: cursor,
        },
    )
    .await
    .is_err()
    {
        return;
    }
    for row in replay {
        match row.into_event() {
            Ok(event) => {
                cursor = cursor.max(event.sequence);
                if send_message(&mut socket, &ServerMessage::Event(event))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                tracing::error!(%error, "stored market-maker event is malformed");
                let _ = send_message(&mut socket, &ServerMessage::ResyncRequired).await;
                return;
            }
        }
    }

    let mut session_check = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { return };
                match message {
                    Message::Text(text) => {
                        if let Err(error) = handle_client_message(
                            &mut socket,
                            &state,
                            &wallet,
                            text.as_str(),
                        ).await {
                            if send_message(&mut socket, &error).await.is_err() {
                                return;
                            }
                        }
                    }
                    Message::Close(_) => return,
                    Message::Ping(payload) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            return;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Binary(_) => {
                        let error = ServerMessage::Error {
                            code: "text_frames_only".into(),
                            message: "send JSON text frames only".into(),
                        };
                        if send_message(&mut socket, &error).await.is_err() {
                            return;
                        }
                    }
                }
            }
            received = events.recv() => {
                match received {
                    Ok(stored) => {
                        if stored.event.sequence <= cursor {
                            continue;
                        }
                        cursor = stored.event.sequence;
                        if stored
                            .audience_wallet
                            .as_deref()
                            .is_some_and(|audience| audience != wallet.as_str())
                        {
                            continue;
                        }
                        if send_message(&mut socket, &ServerMessage::Event(stored.event)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = send_message(&mut socket, &ServerMessage::ResyncRequired).await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            _ = session_check.tick() => {
                match authenticated_market_maker(&state.pool, &token).await {
                    Ok(Some(_)) => {}
                    Ok(None) => return,
                    Err(error) => {
                        tracing::error!(%error, "market-maker session refresh failed");
                        return;
                    }
                }
            }
        }
    }
}

#[derive(sqlx::FromRow)]
struct EventRow {
    sequence: i64,
    kind: String,
    correlation_id: Option<String>,
    payload: String,
}

impl EventRow {
    fn into_event(self) -> Result<GatewayEvent, serde_json::Error> {
        Ok(GatewayEvent {
            sequence: self.sequence,
            kind: self.kind,
            correlation_id: self.correlation_id,
            payload: serde_json::from_str(&self.payload)?,
        })
    }
}

async fn handle_client_message(
    socket: &mut WebSocket,
    state: &GatewayState,
    wallet: &str,
    text: &str,
) -> Result<(), ServerMessage> {
    let message: ClientMessage =
        serde_json::from_str(text).map_err(|_| ServerMessage::Error {
            code: "invalid_message".into(),
            message: "message does not match the gateway protocol".into(),
        })?;
    match message {
        ClientMessage::Quote {
            series,
            sequence,
            bid,
            ask,
        } => {
            if series.is_empty()
                || series.len() > 128
                || sequence <= 0
                || !bid.is_finite()
                || !ask.is_finite()
                || bid <= 0.0
                || ask < bid
            {
                return Err(ServerMessage::Error {
                    code: "invalid_quote".into(),
                    message: "quote requires a series, positive sequence, and 0 < bid <= ask"
                        .into(),
                });
            }
            let updated = sqlx::query(
                "INSERT INTO market_maker_quotes \
                    (wallet_address, series, sequence, bid, ask) VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT(wallet_address, series) DO UPDATE SET \
                    sequence = excluded.sequence, bid = excluded.bid, ask = excluded.ask, \
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                 WHERE excluded.sequence > market_maker_quotes.sequence",
            )
            .bind(wallet)
            .bind(&series)
            .bind(sequence)
            .bind(bid)
            .bind(ask)
            .execute(&state.pool)
            .await
            .map_err(|error| {
                tracing::error!(%error, "market-maker quote persistence failed");
                ServerMessage::Error {
                    code: "quote_unavailable".into(),
                    message: "quote could not be stored".into(),
                }
            })?;
            if updated.rows_affected() == 0 {
                return Err(ServerMessage::Error {
                    code: "stale_quote".into(),
                    message: "quote sequence must increase for this series".into(),
                });
            }

            send_message(
                socket,
                &ServerMessage::QuoteAccepted { series, sequence },
            )
            .await
            .map_err(|_| ServerMessage::Error {
                code: "connection_closed".into(),
                message: "connection closed".into(),
            })
        }
        ClientMessage::OrderIntent(intent) => {
            if state.max_order_notional <= 0.0 {
                return Err(ServerMessage::Error {
                    code: "settlement_unavailable".into(),
                    message: "market-maker order entry is disabled until settlement is configured"
                        .into(),
                });
            }
            let notional = intent.quantity * intent.limit_price;
            if intent.series.is_empty()
                || intent.series.len() > 128
                || !intent.quantity.is_finite()
                || !intent.limit_price.is_finite()
                || intent.quantity <= 0.0
                || intent.limit_price <= 0.0
                || !notional.is_finite()
                || notional > state.max_order_notional
            {
                return Err(ServerMessage::Error {
                    code: "invalid_or_over_limit_order".into(),
                    message: "order intent is invalid or exceeds the configured notional limit"
                        .into(),
                });
            }

            let correlation_id = Uuid::new_v4();
            let serialized = serde_json::to_string(&intent).map_err(|error| {
                tracing::error!(%error, "market-maker order serialization failed");
                ServerMessage::Error {
                    code: "order_unavailable".into(),
                    message: "order intent could not be prepared".into(),
                }
            })?;
            let inserted = sqlx::query(
                "INSERT INTO market_maker_intents \
                    (wallet_address, client_order_id, correlation_id, payload, status) \
                 VALUES (?, ?, ?, ?, 'pending')",
            )
            .bind(wallet)
            .bind(intent.client_order_id.to_string())
            .bind(correlation_id.to_string())
            .bind(&serialized)
            .execute(&state.pool)
            .await;

            let correlation_id = match inserted {
                Ok(_) => correlation_id,
                Err(error) if is_unique_violation(&error) => {
                    let existing = sqlx::query_as::<_, IntentRow>(
                        "SELECT correlation_id, payload, status, settlement_reference \
                         FROM market_maker_intents WHERE wallet_address = ? AND client_order_id = ?",
                    )
                    .bind(wallet)
                    .bind(intent.client_order_id.to_string())
                    .fetch_optional(&state.pool)
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, "market-maker idempotency lookup failed");
                        ServerMessage::Error {
                            code: "idempotency_unavailable".into(),
                            message: "order intent status is temporarily unavailable".into(),
                        }
                    })?
                    .ok_or_else(|| ServerMessage::Error {
                        code: "idempotency_conflict".into(),
                        message: "client order ID is already in use".into(),
                    })?;
                    if existing.payload != serialized {
                        return Err(ServerMessage::Error {
                            code: "idempotency_conflict".into(),
                            message: "client order ID was used for a different order".into(),
                        });
                    }
                    if existing.status == "accepted" {
                        let settlement_reference = existing.settlement_reference.ok_or_else(|| {
                            ServerMessage::Error {
                                code: "order_unavailable".into(),
                                message: "stored accepted order has no settlement reference".into(),
                            }
                        })?;
                        return send_message(
                            socket,
                            &ServerMessage::TradeAccepted {
                                client_order_id: intent.client_order_id,
                                correlation_id: Uuid::parse_str(&existing.correlation_id)
                                    .map_err(|error| {
                                        tracing::error!(%error, "stored market-maker correlation ID is invalid");
                                        ServerMessage::Error {
                                            code: "order_unavailable".into(),
                                            message: "stored order state is invalid".into(),
                                        }
                                    })?,
                                settlement_reference,
                            },
                        )
                        .await
                        .map_err(|_| ServerMessage::Error {
                            code: "connection_closed".into(),
                            message: "connection closed".into(),
                        });
                    }
                    Uuid::parse_str(&existing.correlation_id).map_err(|error| {
                        tracing::error!(%error, "stored market-maker correlation ID is invalid");
                        ServerMessage::Error {
                            code: "order_unavailable".into(),
                            message: "stored order state is invalid".into(),
                        }
                    })?
                }
                Err(error) => {
                    tracing::error!(%error, "market-maker order persistence failed");
                    return Err(ServerMessage::Error {
                        code: "order_unavailable".into(),
                        message: "order intent could not be stored".into(),
                    });
                }
            };

            match state
                .handoff
                .submit(wallet, correlation_id, &intent)
                .await
            {
                Ok(receipt) => {
                    if receipt.settlement_reference.trim().is_empty() {
                        tracing::error!(%correlation_id, "settlement adapter returned an empty reference");
                        return Err(ServerMessage::Error {
                            code: "invalid_settlement_receipt".into(),
                            message: "settlement adapter returned an invalid receipt".into(),
                        });
                    }
                    sqlx::query(
                        "UPDATE market_maker_intents SET status = 'accepted', \
                         settlement_reference = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                         WHERE wallet_address = ? AND client_order_id = ?",
                    )
                    .bind(&receipt.settlement_reference)
                    .bind(wallet)
                    .bind(intent.client_order_id.to_string())
                    .execute(&state.pool)
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, %correlation_id, "market-maker handoff receipt persistence failed");
                        ServerMessage::Error {
                            code: "receipt_persistence_failed".into(),
                            message: "trade was handed off but its receipt could not be persisted".into(),
                        }
                    })?;
                    send_message(
                        socket,
                        &ServerMessage::TradeAccepted {
                            client_order_id: intent.client_order_id,
                            correlation_id,
                            settlement_reference: receipt.settlement_reference,
                        },
                    )
                    .await
                    .map_err(|_| ServerMessage::Error {
                        code: "connection_closed".into(),
                        message: "connection closed".into(),
                    })
                }
                Err(_) => {
                    sqlx::query(
                        "UPDATE market_maker_intents SET status = 'rejected', \
                         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                         WHERE wallet_address = ? AND client_order_id = ?",
                    )
                    .bind(wallet)
                    .bind(intent.client_order_id.to_string())
                    .execute(&state.pool)
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, %correlation_id, "market-maker failure persistence failed");
                        ServerMessage::Error {
                            code: "order_status_unavailable".into(),
                            message: "order handoff failed and its status could not be persisted".into(),
                        }
                    })?;
                    send_message(
                        socket,
                        &ServerMessage::TradeRejected {
                            client_order_id: intent.client_order_id,
                            correlation_id,
                            reason: "settlement_rejected".into(),
                        },
                    )
                    .await
                    .map_err(|_| ServerMessage::Error {
                        code: "connection_closed".into(),
                        message: "connection closed".into(),
                    })
                }
            }
        }
    }
}

#[derive(sqlx::FromRow)]
struct IntentRow {
    correlation_id: String,
    payload: String,
    status: String,
    settlement_reference: Option<String>,
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db_error) if db_error.is_unique_violation())
}

async fn send_message(
    socket: &mut WebSocket,
    message: &ServerMessage,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(message).expect("gateway messages always serialize");
    socket.send(Message::Text(text)).await
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}
