//! Production-grade internal Soroban RPC client.
//!
//! Wraps the JSON-RPC 2.0 methods exposed by Stellar's Soroban RPC
//! (`getHealth`, `getLatestLedger`, `getLedgerEntries`, `getEvents`,
//! `simulateTransaction`, `sendTransaction`, `getTransaction`) with typed
//! request/response structs, exponential-backoff retries, endpoint failover
//! and per-method latency/error metrics.
//!
//! Retries are only applied to idempotent reads and transient errors.
//! `sendTransaction` is never blindly retried: the caller is responsible for
//! resubmission by transaction hash.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::RwLock;

/// Default number of ledgers an endpoint may lag behind the best known
/// ledger before it is considered unhealthy.
pub const DEFAULT_MAX_LEDGER_LAG: u32 = 5;

/// Default per-request timeout.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Default number of attempts (initial try + retries) for idempotent reads.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Base delay used for exponential backoff between retries.
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(200);

/// Typed errors produced by the Soroban RPC client.
#[derive(Debug, Error)]
pub enum RpcError {
    /// The underlying HTTP transport failed (connection, timeout, ...).
    #[error("rpc transport error: {0}")]
    Transport(String),

    /// The endpoint responded with HTTP 429 or an equivalent rate-limit signal.
    #[error("rpc rate limited: {0}")]
    RateLimited(String),

    /// The endpoint is behind the best known ledger by more than the allowed lag.
    #[error("rpc node behind: endpoint ledger {endpoint_ledger}, best known {best_ledger}")]
    NodeBehind {
        endpoint_ledger: u32,
        best_ledger: u32,
    },

    /// The endpoint returned a JSON-RPC 2.0 error object.
    #[error("json-rpc error {code}: {message}")]
    JsonRpc { code: i64, message: String },

    /// The response body could not be decoded into the expected type.
    #[error("rpc decode error: {0}")]
    Decode(String),

    /// No healthy endpoint was available to serve the request.
    #[error("no healthy rpc endpoint available")]
    NoHealthyEndpoint,
}

impl RpcError {
    /// Whether the error is transient and the request may be retried.
    ///
    /// Only transport failures, rate limiting and node-behind conditions are
    /// considered transient. JSON-RPC errors and decode errors are terminal.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            RpcError::Transport(_) | RpcError::RateLimited(_) | RpcError::NodeBehind { .. }
        )
    }
}

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest<'a> {
    pub jsonrpc: &'a str,
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

impl<'a> JsonRpcRequest<'a> {
    pub fn new(id: u64, method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse {
    pub id: Option<u64>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcErrorObject>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// Result of `getHealth`.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    #[serde(default)]
    pub oldest_ledger: Option<u32>,
}

/// Result of `getLatestLedger`.
#[derive(Debug, Clone, Deserialize)]
pub struct LatestLedgerResponse {
    pub id: String,
    pub sequence: u32,
    #[serde(default)]
    pub protocol_version: Option<u32>,
}

/// A single ledger entry key/value pair.
#[derive(Debug, Clone, Deserialize)]
pub struct LedgerEntry {
    pub key: String,
    pub xdr: String,
    #[serde(default)]
    pub last_modified_ledger_seq: Option<u32>,
    #[serde(default)]
    pub live_until_ledger_seq: Option<u32>,
}

/// Result of `getLedgerEntries`.
#[derive(Debug, Clone, Deserialize)]
pub struct LedgerEntriesResponse {
    #[serde(default)]
    pub entries: Vec<LedgerEntry>,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
}

/// A single contract event.
#[derive(Debug, Clone, Deserialize)]
pub struct ContractEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub ledger: u32,
    #[serde(default)]
    pub ledger_closed_at: Option<String>,
    pub contract_id: String,
    #[serde(default)]
    pub topic: Vec<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub tx_hash: Option<String>,
}

/// Result of `getEvents`, including the pagination cursor.
#[derive(Debug, Clone, Deserialize)]
pub struct EventsResponse {
    #[serde(default)]
    pub events: Vec<ContractEvent>,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Result of `simulateTransaction`.
#[derive(Debug, Clone, Deserialize)]
pub struct SimulateTransactionResponse {
    #[serde(default)]
    pub transaction_data: Option<String>,
    #[serde(default)]
    pub min_resource_fee: Option<String>,
    #[serde(default)]
    pub results: Vec<Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
}

/// Result of `sendTransaction`.
#[derive(Debug, Clone, Deserialize)]
pub struct SendTransactionResponse {
    pub status: String,
    pub hash: String,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    #[serde(default)]
    pub error_result_xdr: Option<String>,
}

/// Result of `getTransaction`.
#[derive(Debug, Clone, Deserialize)]
pub struct GetTransactionResponse {
    pub status: String,
    #[serde(default)]
    pub ledger: Option<u32>,
    #[serde(default)]
    pub envelope_xdr: Option<String>,
    #[serde(default)]
    pub result_xdr: Option<String>,
    #[serde(default)]
    pub result_meta_xdr: Option<String>,
}

/// Per-method latency and error metrics.
#[derive(Debug, Default)]
pub struct MethodMetrics {
    pub calls: AtomicU64,
    pub errors: AtomicU64,
    pub total_latency_micros: AtomicU64,
}

impl MethodMetrics {
    fn record(&self, latency: Duration, failed: bool) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.total_latency_micros
            .fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
    }

    /// Average latency in milliseconds, or `None` if the method was never called.
    pub fn avg_latency_ms(&self) -> Option<f64> {
        let calls = self.calls.load(Ordering::Relaxed);
        if calls == 0 {
            return None;
        }
        let total = self.total_latency_micros.load(Ordering::Relaxed);
        Some((total as f64 / calls as f64) / 1000.0)
    }
}

/// Aggregated metrics for all RPC methods.
#[derive(Debug, Default)]
pub struct RpcMetrics {
    pub methods: HashMap<&'static str, MethodMetrics>,
}

impl RpcMetrics {
    fn new() -> Self {
        let mut methods = HashMap::new();
        for name in [
            "getHealth",
            "getLatestLedger",
            "getLedgerEntries",
            "getEvents",
            "simulateTransaction",
            "sendTransaction",
            "getTransaction",
        ] {
            methods.insert(name, MethodMetrics::default());
        }
        Self { methods }
    }

    fn record(&self, method: &str, latency: Duration, failed: bool) {
        if let Some(m) = self.methods.get(method) {
            m.record(latency, failed);
        }
    }
}

/// Health state of a single endpoint.
#[derive(Debug, Clone)]
struct EndpointState {
    url: String,
    healthy: bool,
    last_ledger: Option<u32>,
}

/// Configuration for the Soroban RPC client.
#[derive(Debug, Clone)]
pub struct SorobanRpcConfig {
    pub endpoints: Vec<String>,
    pub max_ledger_lag: u32,
    pub request_timeout: Duration,
    pub max_attempts: u32,
    pub backoff_base: Duration,
}

impl SorobanRpcConfig {
    pub fn new(endpoints: Vec<String>) -> Self {
        Self {
            endpoints,
            max_ledger_lag: DEFAULT_MAX_LEDGER_LAG,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_base: DEFAULT_BACKOFF_BASE,
        }
    }
}

/// Abstraction over the Soroban RPC so tests can substitute a mock.
#[async_trait::async_trait]
pub trait SorobanRpc: Send + Sync {
    async fn get_health(&self) -> Result<HealthResponse, RpcError>;
    async fn get_latest_ledger(&self) -> Result<LatestLedgerResponse, RpcError>;
    async fn get_ledger_entries(&self, keys: Vec<String>) -> Result<LedgerEntriesResponse, RpcError>;
    async fn get_events(
        &self,
        start_ledger: u32,
        cursor: Option<String>,
    ) -> Result<EventsResponse, RpcError>;
    async fn simulate_transaction(&self, tx_xdr: String) -> Result<SimulateTransactionResponse, RpcError>;
    async fn send_transaction(&self, tx_xdr: String) -> Result<SendTransactionResponse, RpcError>;
    async fn get_transaction(&self, hash: &str) -> Result<GetTransactionResponse, RpcError>;
}

/// Concrete HTTP-backed Soroban RPC client with failover and retries.
pub struct HttpSorobanRpc {
    client: reqwest::Client,
    config: SorobanRpcConfig,
    endpoints: RwLock<Vec<EndpointState>>,
    metrics: Arc<RpcMetrics>,
    next_id: AtomicU64,
}

impl HttpSorobanRpc {
    pub fn new(config: SorobanRpcConfig) -> Result<Self, RpcError> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        let endpoints = config
            .endpoints
            .iter()
            .map(|url| EndpointState {
                url: url.clone(),
                healthy: true,
                last_ledger: None,
            })
            .collect();
        Ok(Self {
            client,
            config,
            endpoints: RwLock::new(endpoints),
            metrics: Arc::new(RpcMetrics::new()),
            next_id: AtomicU64::new(1),
        })
    }

    /// Shared metrics handle for observability.
    pub fn metrics(&self) -> Arc<RpcMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Whether the RPC layer currently has at least one healthy endpoint.
    pub async fn is_healthy(&self) -> bool {
        self.endpoints.read().await.iter().any(|e| e.healthy)
    }

    /// Best known ledger across all endpoints.
    async fn best_ledger(&self) -> Option<u32> {
        self.endpoints
            .read()
            .await
            .iter()
            .filter_map(|e| e.last_ledger)
            .max()
    }

    /// Update an endpoint's observed ledger and recompute its health relative
    /// to the best known ledger across all endpoints.
    async fn observe_ledger(&self, url: &str, ledger: u32) {
        let mut endpoints = self.endpoints.write().await;
        if let Some(ep) = endpoints.iter_mut().find(|e| e.url == url) {
            ep.last_ledger = Some(ledger);
        }
        let best = endpoints.iter().filter_map(|e| e.last_ledger).max();
        if let Some(best) = best {
            for ep in endpoints.iter_mut() {
                ep.healthy = match ep.last_ledger {
                    Some(l) => best.saturating_sub(l) <= self.config.max_ledger_lag,
                    None => ep.healthy,
                };
            }
        }
    }

    /// Mark an endpoint unhealthy after a transport failure.
    async fn mark_unhealthy(&self, url: &str) {
        let mut endpoints = self.endpoints.write().await;
        if let Some(ep) = endpoints.iter_mut().find(|e| e.url == url) {
            ep.healthy = false;
        }
    }

    /// Ordered list of candidate endpoints, healthy ones first.
    async fn candidates(&self) -> Vec<String> {
        let endpoints = self.endpoints.read().await;
        let mut healthy: Vec<String> = endpoints
            .iter()
            .filter(|e| e.healthy)
            .map(|e| e.url.clone())
            .collect();
        if healthy.is_empty() {
            // Fall back to all endpoints so a transient outage can recover.
            healthy = endpoints.iter().map(|e| e.url.clone()).collect();
        }
        healthy
    }

    /// Perform a single JSON-RPC call against one endpoint.
    async fn call_once<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);
        let response = self
            .client
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(RpcError::RateLimited(url.to_string()));
        }
        if !status.is_success() {
            return Err(RpcError::Transport(format!("http {status}")));
        }

        let body: JsonRpcResponse = response
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.to_string()))?;

        if let Some(err) = body.error {
            return Err(RpcError::JsonRpc {
                code: err.code,
                message: err.message,
            });
        }

        let result = body
            .result
            .ok_or_else(|| RpcError::Decode("missing result".to_string()))?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(e.to_string()))
    }

    /// Execute an idempotent read with retries and endpoint failover.
    async fn call_read<T: for<'de> Deserialize<'de>>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<T, RpcError> {
        let candidates = self.candidates().await;
        if candidates.is_empty() {
            return Err(RpcError::NoHealthyEndpoint);
        }

        let mut last_err: Option<RpcError> = None;
        for attempt in 0..self.config.max_attempts {
            let url = candidates[(attempt as usize) % candidates.len()].clone();
            let started = Instant::now();
            let outcome = self.call_once::<T>(&url, method, params.clone()).await;
            let latency = started.elapsed();
            self.metrics.record(method, latency, outcome.is_err());

            match outcome {
                Ok(value) => return Ok(value),
                Err(err) => {
                    if matches!(err, RpcError::Transport(_)) {
                        self.mark_unhealthy(&url).await;
                    }
                    let transient = err.is_transient();
                    last_err = Some(err);
                    if !transient || attempt + 1 >= self.config.max_attempts {
                        break;
                    }
                    let backoff = self.config.backoff_base * 2u32.pow(attempt);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
        Err(last_err.unwrap_or(RpcError::NoHealthyEndpoint))
    }
}

#[async_trait::async_trait]
impl SorobanRpc for HttpSorobanRpc {
    async fn get_health(&self) -> Result<HealthResponse, RpcError> {
        let health: HealthResponse = self.call_read("getHealth", json!({})).await?;
        if let Some(ledger) = health.latest_ledger {
            let url = self.candidates().await.into_iter().next();
            if let Some(url) = url {
                self.observe_ledger(&url, ledger).await;
            }
        }
        Ok(health)
    }

    async fn get_latest_ledger(&self) -> Result<LatestLedgerResponse, RpcError> {
        let latest: LatestLedgerResponse = self.call_read("getLatestLedger", json!({})).await?;
        let url = self.candidates().await.into_iter().next();
        if let Some(url) = url {
            self.observe_ledger(&url, latest.sequence).await;
        }
        Ok(latest)
    }

    async fn get_ledger_entries(&self, keys: Vec<String>) -> Result<LedgerEntriesResponse, RpcError> {
        self.call_read("getLedgerEntries", json!({ "keys": keys }))
            .await
    }

    async fn get_events(
        &self,
        start_ledger: u32,
        cursor: Option<String>,
    ) -> Result<EventsResponse, RpcError> {
        let mut params = json!({ "startLedger": start_ledger });
        if let Some(cursor) = cursor {
            params["cursor"] = Value::String(cursor);
        }
        self.call_read("getEvents", params).await
    }

    async fn simulate_transaction(&self, tx_xdr: String) -> Result<SimulateTransactionResponse, RpcError> {
        self.call_read("simulateTransaction", json!({ "transaction": tx_xdr }))
            .await
    }

    async fn send_transaction(&self, tx_xdr: String) -> Result<SendTransactionResponse, RpcError> {
        // Never blindly retried: the caller resubmits by hash if needed.
        let candidates = self.candidates().await;
        let url = candidates.first().ok_or(RpcError::NoHealthyEndpoint)?;
        let started = Instant::now();
        let outcome = self
            .call_once::<SendTransactionResponse>(url, "sendTransaction", json!({ "transaction": tx_xdr }))
            .await;
        self.metrics
            .record("sendTransaction", started.elapsed(), outcome.is_err());
        outcome
    }

    async fn get_transaction(&self, hash: &str) -> Result<GetTransactionResponse, RpcError> {
        self.call_read("getTransaction", json!({ "hash": hash })).await
    }
}
