//! Typed request/response structures for the Soroban RPC client.
//!
//! These mirror the JSON-RPC 2.0 shapes returned by Stellar RPC
//! (<https://developers.stellar.org/docs/data/apis/rpc/api-reference>).

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest<P> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: P,
}

impl<P> JsonRpcRequest<P> {
    pub fn new(id: u64, method: &'static str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse<R> {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default)]
    pub result: Option<R>,
    #[serde(default)]
    pub error: Option<JsonRpcErrorObject>,
}

/// The `error` member of a JSON-RPC 2.0 response.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// Result of `getHealth`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: String,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    #[serde(default)]
    pub oldest_ledger: Option<u32>,
}

/// Result of `getLatestLedger`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestLedgerResponse {
    pub id: String,
    pub sequence: u32,
    #[serde(default)]
    pub protocol_version: Option<u32>,
}

/// A single ledger entry key, base64-encoded XDR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerKey {
    pub xdr: String,
}

/// Params for `getLedgerEntries`.
#[derive(Debug, Clone, Serialize)]
pub struct GetLedgerEntriesParams {
    pub keys: Vec<String>,
}

/// A single ledger entry returned by `getLedgerEntries`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntryResult {
    pub key: String,
    pub xdr: String,
    #[serde(default)]
    pub last_modified_ledger_seq: Option<u32>,
    #[serde(default)]
    pub live_until_ledger_seq: Option<u32>,
}

/// Result of `getLedgerEntries`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetLedgerEntriesResponse {
    #[serde(default)]
    pub entries: Vec<LedgerEntryResult>,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
}

/// Params for `getEvents`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsParams {
    pub start_ledger: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ledger: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_ids: Option<Vec<String>>,
}

/// A single event returned by `getEvents`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventResult {
    #[serde(rename = "type")]
    pub kind: String,
    pub ledger: u32,
    #[serde(default)]
    pub ledger_closed_at: Option<String>,
    pub contract_id: String,
    pub id: String,
    #[serde(default)]
    pub topic: Vec<String>,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub tx_hash: Option<String>,
}

/// Result of `getEvents`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsResponse {
    #[serde(default)]
    pub events: Vec<EventResult>,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Params for `simulateTransaction`.
#[derive(Debug, Clone, Serialize)]
pub struct SimulateTransactionParams {
    pub transaction: String,
}

/// The ledger footprint a simulated transaction reads from and writes to.
///
/// A Soroban transaction is only valid for the ledger entries it declared in
/// its footprint, so the footprint has to travel with the envelope.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SimulateFootprint {
    /// Ledger entries the transaction reads.
    #[serde(default)]
    pub read: Vec<LedgerKey>,
    /// Ledger entries the transaction writes.
    #[serde(default)]
    pub write: Vec<LedgerKey>,
}

impl SimulateFootprint {
    /// Number of ledger entries covered by the footprint.
    pub fn len(&self) -> usize {
        self.read.len() + self.write.len()
    }

    /// Whether the footprint covers no ledger entry at all.
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }
}

/// The resource requirements reported by a successful `simulateTransaction`.
///
/// `instructions`, `read_bytes`, `write_bytes` and `events` are the declared
/// resource consumption of the transaction; the footprint lists the ledger
/// entries the transaction touches.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SimulateResources {
    /// CPU instructions the host will execute.
    #[serde(default)]
    pub instructions: u32,
    /// Bytes read from ledger entries.
    #[serde(default)]
    pub read_bytes: u32,
    /// Bytes written to ledger entries.
    #[serde(default)]
    pub write_bytes: u32,
    /// Contract events emitted.
    #[serde(default)]
    pub events: u32,
    /// The read/write ledger footprint.
    #[serde(default)]
    pub footprint: Option<SimulateFootprint>,
    /// Set when the node reports that archived entries must be restored
    /// before the transaction can be submitted.
    #[serde(default)]
    pub restore_footprint_required: bool,
    /// The entries that have to be restored (present when the node reports
    /// archived entries).
    #[serde(default)]
    pub restore_footprint: Option<SimulateFootprint>,
}

/// Result of `simulateTransaction`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulateTransactionResponse {
    #[serde(default)]
    pub transaction_data: Option<String>,
    #[serde(default)]
    pub min_resource_fee: Option<String>,
    #[serde(default)]
    pub results: Vec<SimulateTransactionResult>,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    #[serde(default)]
    pub error: Option<String>,
    /// Resource consumption and ledger footprint of the simulated
    /// transaction.
    #[serde(default)]
    pub resources: Option<SimulateResources>,
}

/// Result of `getFeeStats`.
///
/// The node reports the distribution of inclusion fees paid by the transactions
/// it has seen recently, which is what the fee strategy bids against. The
/// amounts are accepted both as a number and as a string, because the node
/// reports them as strings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GetFeeStatsResponse {
    /// Ledger the statistics were computed at.
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    /// Oldest ledger still covered by the statistics.
    #[serde(default)]
    pub oldest_ledger: Option<u32>,
    /// Highest inclusion fee observed in the window.
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub max_fee: Option<i64>,
    /// Lowest inclusion fee observed in the window.
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub min_fee: Option<i64>,
    /// Median inclusion fee.
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub p50: Option<i64>,
    /// 75th percentile inclusion fee.
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub p75: Option<i64>,
    /// 90th percentile inclusion fee.
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub p90: Option<i64>,
    /// 95th percentile inclusion fee.
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub p95: Option<i64>,
    /// 99th percentile inclusion fee.
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub p99: Option<i64>,
    /// Number of transactions in the sample.
    #[serde(default)]
    pub transaction_count: Option<u64>,
    /// Number of ledgers in the sample.
    #[serde(default)]
    pub ledger_count: Option<u64>,
}

/// Read a fee amount that the node reports either as a number or as a string.
fn deserialize_amount<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Amount {
        Number(i64),
        Text(String),
    }

    match Option::<Amount>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Amount::Number(amount)) => Ok(Some(amount)),
        Some(Amount::Text(text)) => text
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// A single result entry within a `simulateTransaction` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulateTransactionResult {
    #[serde(default)]
    pub xdr: Option<String>,
    #[serde(default)]
    pub auth: Vec<String>,
}

/// Params for `sendTransaction`.
#[derive(Debug, Clone, Serialize)]
pub struct SendTransactionParams {
    pub transaction: String,
}

/// Result of `sendTransaction`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTransactionResponse {
    pub status: String,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub latest_ledger: Option<u32>,
    #[serde(default)]
    pub error_result_xdr: Option<String>,
}

/// Params for `getTransaction`.
#[derive(Debug, Clone, Serialize)]
pub struct GetTransactionParams {
    pub hash: String,
}

/// Result of `getTransaction`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    #[serde(default)]
    pub latest_ledger: Option<u32>,
}
