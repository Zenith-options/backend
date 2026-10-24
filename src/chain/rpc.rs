//! Soroban JSON-RPC client.
//!
//! Two clients live here:
//!
//! * [`RpcClient`] talks to a real node over JSON-RPC 2.0. It is the client
//!   used in production by the fee oracle, the simulator and the indexer.
//! * [`SorobanRpcClient`] is the in-process mock kept for the keeper and the
//!   unit tests; it never touches the network.
//!
//! Everything the platform needs from a node goes through the [`SorobanRpc`]
//! trait, so tests can inject deterministic responses instead of talking to
//! testnet.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::types::{
    EventResult, GetEventsParams, GetEventsResponse, GetFeeStatsResponse, SendTransactionResponse,
    SimulateTransactionResponse,
};

/// A ledger entry key with its live-until information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntryInfo {
    pub key: String,
    pub live_until_ledger_seq: Option<u32>, // None if archived
    pub is_archived: bool,
}

/// Errors surfaced by the Soroban RPC client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The request never reached the node (DNS, TLS, connection reset, ...).
    Transport(String),
    /// The response was not the JSON we expected.
    Decode(String),
    /// The node returned a JSON-RPC error object.
    JsonRpc { code: i64, message: String },
    /// The node is rate limiting us; the caller should back off and retry.
    RateLimited,
    /// The node does not implement the method (older testnet nodes do not
    /// serve `getFeeStats`).
    MethodUnavailable(String),
    /// The response was well formed but semantically unexpected.
    Unexpected(String),
}

impl RpcError {
    /// Whether retrying the same request could plausibly succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::RateLimited)
    }

    /// Whether the node simply does not serve this data. Callers treat this
    /// as "no data available" rather than as a failure — for example the fee
    /// strategy falls back to its floor when fee statistics are missing.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::MethodUnavailable(_))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "Soroban RPC transport error: {msg}"),
            Self::Decode(msg) => write!(f, "Soroban RPC decode error: {msg}"),
            Self::JsonRpc { code, message } => {
                write!(f, "Soroban RPC error {code}: {message}")
            }
            Self::RateLimited => write!(f, "Soroban RPC rate limited"),
            Self::MethodUnavailable(method) => {
                write!(f, "Soroban RPC method not available: {method}")
            }
            Self::Unexpected(msg) => write!(f, "Unexpected Soroban RPC response: {msg}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// The subset of the Soroban RPC surface the platform depends on.
///
/// Keeping it behind a trait is what makes the fee strategy, the simulator
/// and the indexer testable without a node.
#[async_trait]
pub trait SorobanRpc: Send + Sync {
    /// Sequence of the latest closed ledger.
    async fn latest_ledger(&self) -> Result<u32, RpcError>;

    /// Simulate a base64 transaction envelope.
    async fn simulate_transaction(
        &self,
        envelope_xdr: &str,
    ) -> Result<SimulateTransactionResponse, RpcError>;

    /// Recent inclusion-fee statistics.
    async fn get_fee_stats(&self) -> Result<GetFeeStatsResponse, RpcError>;

    /// Paginated contract events.
    async fn get_events(&self, params: &GetEventsParams) -> Result<GetEventsResponse, RpcError>;

    /// Submit a signed transaction envelope.
    async fn send_transaction(
        &self,
        envelope_xdr: &str,
    ) -> Result<SendTransactionResponse, RpcError>;
}

/// A JSON-RPC 2.0 client for a Soroban node.
#[derive(Clone)]
pub struct RpcClient {
    endpoint: String,
    http: reqwest::Client,
    request_timeout: Duration,
}

impl RpcClient {
    /// Create a client for the given RPC endpoint URL.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            http: reqwest::Client::new(),
            request_timeout: Duration::from_secs(10),
        }
    }

    /// Override the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// The endpoint this client talks to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Issue a JSON-RPC call and return the `result` member.
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        let response = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(RpcError::RateLimited);
        }
        if !status.is_success() {
            return Err(RpcError::Transport(format!("http {status}")));
        }

        let value: Value = response
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.to_string()))?;

        if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown rpc error")
                .to_string();
            // -32601 is the JSON-RPC "method not found" code: the node is
            // simply too old to serve the method (e.g. `getFeeStats`).
            let kind = if code == -32601 {
                RpcError::MethodUnavailable(method.to_string())
            } else {
                RpcError::JsonRpc { code, message }
            };
            return Err(kind);
        }

        value
            .get("result")
            .cloned()
            .ok_or_else(|| RpcError::Decode("missing result in json-rpc response".into()))
    }
}

#[async_trait]
impl SorobanRpc for RpcClient {
    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        let result = self.call("getLatestLedger", json!({})).await?;
        result
            .get("sequence")
            .and_then(Value::as_u64)
            .map(|sequence| sequence as u32)
            .ok_or_else(|| RpcError::Unexpected("missing latest ledger sequence".into()))
    }

    async fn simulate_transaction(
        &self,
        envelope_xdr: &str,
    ) -> Result<SimulateTransactionResponse, RpcError> {
        let result = self
            .call(
                "simulateTransaction",
                json!({ "transaction": envelope_xdr }),
            )
            .await?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(e.to_string()))
    }

    async fn get_fee_stats(&self) -> Result<GetFeeStatsResponse, RpcError> {
        let result = self.call("getFeeStats", json!({})).await?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(e.to_string()))
    }

    async fn get_events(&self, params: &GetEventsParams) -> Result<GetEventsResponse, RpcError> {
        let params = serde_json::to_value(params).map_err(|e| RpcError::Decode(e.to_string()))?;
        let result = self.call("getEvents", params).await?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(e.to_string()))
    }

    async fn send_transaction(
        &self,
        envelope_xdr: &str,
    ) -> Result<SendTransactionResponse, RpcError> {
        let result = self
            .call("sendTransaction", json!({ "transaction": envelope_xdr }))
            .await?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(e.to_string()))
    }
}

/// A scripted Soroban node used by tests and by the sandbox harness.
///
/// Responses are recorded up-front so a test can replay a captured node
/// interaction (a simulation, a fee-statistics sample, a page of events)
/// without any network access.
#[derive(Debug, Default)]
pub struct MockSorobanRpc {
    state: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    latest_ledger: u32,
    simulation: Option<Result<SimulateTransactionResponse, RpcError>>,
    fee_stats: Option<Result<GetFeeStatsResponse, RpcError>>,
    events: Vec<Result<GetEventsResponse, RpcError>>,
    /// When set, the node serves one event per ledger of `[from, to]`.
    ledger_events: Option<(u32, u32)>,
    /// The oldest ledger the node still serves events for.
    retention_floor: Option<u32>,
    sent: Vec<String>,
}

/// A contract event the mock node serves for a ledger.
///
/// The `value` carries the decoded event data, which is what the projection
/// step consumes.
fn mock_event(ledger: u32) -> EventResult {
    EventResult {
        kind: "OptionMinted".to_string(),
        ledger,
        ledger_closed_at: None,
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM".to_string(),
        id: format!("{ledger:08}-0"),
        topic: vec!["mint".to_string()],
        value: serde_json::json!({
            "series_id": format!("SERIES-{ledger}"),
            "strike": 0.12,
            "expiry": 1_700_000_000u64,
            "is_call": true,
            "contracts": 10.0,
            "writer": "GWRITER",
        })
        .to_string(),
        tx_hash: Some(format!("tx-{ledger}")),
    }
}

impl MockSorobanRpc {
    /// An empty mock reporting ledger `latest_ledger`.
    pub fn new(latest_ledger: u32) -> Self {
        Self {
            state: Mutex::new(MockState {
                latest_ledger,
                ..MockState::default()
            }),
        }
    }

    /// Script the response of `simulateTransaction`.
    pub fn with_simulation(
        &self,
        response: Result<SimulateTransactionResponse, RpcError>,
    ) -> &Self {
        self.lock().simulation = Some(response);
        self
    }

    /// Script the response of `getFeeStats`.
    pub fn with_fee_stats(&self, response: Result<GetFeeStatsResponse, RpcError>) -> &Self {
        self.lock().fee_stats = Some(response);
        self
    }

    /// Script the pages returned by `getEvents`, in order.
    pub fn with_event_pages(&self, pages: Vec<Result<GetEventsResponse, RpcError>>) -> &Self {
        self.lock().events = pages;
        self
    }

    /// Serve one event per ledger of `[from, to]`.
    ///
    /// The events are generated on demand, so the mock behaves like a node
    /// that paginates over a real ledger range.
    pub fn with_ledger_events(&self, from: u32, to: u32) -> &Self {
        self.lock().ledger_events = Some((from, to));
        self
    }

    /// Refuse to serve events older than `ledger`.
    ///
    /// This is what a node does once its event retention window has moved
    /// past a range.
    pub fn with_retention_floor(&self, ledger: u32) -> &Self {
        self.lock().retention_floor = Some(ledger);
        self
    }

    /// The envelopes handed to `sendTransaction`, in order.
    pub fn submitted_envelopes(&self) -> Vec<String> {
        self.lock().sent.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl SorobanRpc for MockSorobanRpc {
    async fn latest_ledger(&self) -> Result<u32, RpcError> {
        Ok(self.lock().latest_ledger)
    }

    async fn simulate_transaction(
        &self,
        _envelope_xdr: &str,
    ) -> Result<SimulateTransactionResponse, RpcError> {
        self.lock()
            .simulation
            .clone()
            .unwrap_or_else(|| Err(RpcError::Unexpected("no scripted simulation".into())))
    }

    async fn get_fee_stats(&self) -> Result<GetFeeStatsResponse, RpcError> {
        self.lock()
            .fee_stats
            .clone()
            .unwrap_or_else(|| Err(RpcError::MethodUnavailable("getFeeStats".into())))
    }

    async fn get_events(&self, params: &GetEventsParams) -> Result<GetEventsResponse, RpcError> {
        let mut state = self.lock();

        if let Some(floor) = state.retention_floor {
            if params.start_ledger < floor {
                return Err(RpcError::JsonRpc {
                    code: -32602,
                    message: format!(
                        "ledger {} is outside the retention window (oldest served: {floor})",
                        params.start_ledger
                    ),
                });
            }
        }

        if let Some((from, to)) = state.ledger_events {
            let mut start = params.start_ledger.max(from);
            if let Some(parsed) = params
                .cursor
                .as_deref()
                .and_then(|cursor| cursor.parse::<u32>().ok())
            {
                start = start.max(parsed.saturating_add(1));
            }
            let end = params.end_ledger.unwrap_or(to).min(to);
            let limit = params.limit.unwrap_or(u32::MAX) as usize;

            let mut events = Vec::new();
            let mut ledger = start;
            while ledger <= end && events.len() < limit {
                events.push(mock_event(ledger));
                ledger += 1;
            }
            let cursor = if ledger <= end {
                Some((ledger.saturating_sub(1)).to_string())
            } else {
                None
            };
            return Ok(GetEventsResponse {
                events,
                latest_ledger: Some(state.latest_ledger),
                cursor,
            });
        }

        if state.events.is_empty() {
            return Ok(GetEventsResponse::default());
        }
        state.events.remove(0)
    }

    async fn send_transaction(
        &self,
        envelope_xdr: &str,
    ) -> Result<SendTransactionResponse, RpcError> {
        let mut state = self.lock();
        state.sent.push(envelope_xdr.to_string());
        Ok(SendTransactionResponse {
            status: "PENDING".to_string(),
            hash: Some("mock-hash".to_string()),
            latest_ledger: Some(state.latest_ledger),
            error_result_xdr: None,
        })
    }
}

/// In-process mock kept for the keeper and the unit tests.
///
/// It answers ledger-entry lookups from an in-memory table and never touches
/// the network.
#[derive(Clone)]
pub struct SorobanRpcClient {
    pub rpc_url: String,
    mock_entries: Arc<Mutex<HashMap<String, LedgerEntryInfo>>>,
}

impl SorobanRpcClient {
    pub fn new(rpc_url: String) -> Self {
        Self {
            rpc_url,
            mock_entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn insert_mock_entry(&self, key: &str, live_until: Option<u32>, is_archived: bool) {
        let mut entries = self.mock_entries.lock().unwrap();
        entries.insert(
            key.to_string(),
            LedgerEntryInfo {
                key: key.to_string(),
                live_until_ledger_seq: live_until,
                is_archived,
            },
        );
    }

    pub async fn get_latest_ledger(&self) -> Result<u32, String> {
        Ok(1_000_000)
    }

    pub async fn get_ledger_entries(
        &self,
        keys: &[String],
    ) -> Result<Vec<LedgerEntryInfo>, String> {
        let entries = self.mock_entries.lock().unwrap();
        let mut result = Vec::new();
        for k in keys {
            if let Some(entry) = entries.get(k) {
                result.push(entry.clone());
            } else {
                result.push(LedgerEntryInfo {
                    key: k.clone(),
                    live_until_ledger_seq: Some(1_500_000),
                    is_archived: false,
                });
            }
        }
        Ok(result)
    }
}
