//! Minimal Soroban RPC client used by the reconciliation job.
//!
//! The reconciliation job needs point reads of contract data (option token
//! balances, vault collateral, series state) at a fixed ledger sequence so the
//! on-chain snapshot is consistent with the off-chain projection snapshot.
//! `getLedgerEntries` is the RPC method that provides those point reads.
//!
//! This module intentionally stays small: it only wraps the RPC calls the
//! reconciler needs and keeps batching/rate-limit concerns in one place.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::chain::errors::ChainError;

/// Maximum number of ledger keys accepted per `getLedgerEntries` call.
///
/// The Soroban RPC enforces a hard cap; the reconciler batches keys so it never
/// exceeds this limit regardless of how many wallets/series are being checked.
pub const MAX_LEDGER_KEYS_PER_CALL: usize = 200;

/// A single ledger key to read, encoded as the base64 XDR string the RPC
/// expects. Callers build these from contract id + ledger key XDR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerKey {
    /// Base64-encoded `LedgerKey` XDR.
    pub key: String,
}

/// One entry returned by `getLedgerEntries`.
#[derive(Debug, Clone, Deserialize)]
pub struct LedgerEntryResult {
    /// Base64-encoded `LedgerEntry` XDR, present when the key exists on-chain.
    #[serde(default)]
    pub xdr: Option<String>,
    /// The ledger sequence at which this entry was last modified.
    #[serde(default, rename = "lastModifiedLedgerSeq")]
    pub last_modified_ledger_seq: Option<u32>,
    /// Echo of the requested key, so callers can map results back to requests.
    #[serde(default)]
    pub key: Option<String>,
}

/// Response envelope for `getLedgerEntries`.
#[derive(Debug, Clone, Deserialize)]
pub struct GetLedgerEntriesResponse {
    #[serde(default)]
    pub entries: Vec<LedgerEntryResult>,
    /// Latest ledger known to the RPC at the time of the call.
    #[serde(default, rename = "latestLedger")]
    pub latest_ledger: Option<u32>,
}

/// Client for the Soroban RPC endpoint.
#[derive(Debug, Clone)]
pub struct RpcClient {
    endpoint: String,
    http: reqwest::Client,
}

impl RpcClient {
    /// Create a client for the given RPC endpoint URL.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Read a batch of ledger entries at a fixed ledger sequence.
    ///
    /// `keys` is split into chunks of at most [`MAX_LEDGER_KEYS_PER_CALL`] so
    /// the RPC's per-call limit is never exceeded. When `ledger_seq` is set the
    /// request is pinned to that sequence, giving the reconciler a consistent
    /// snapshot of on-chain state.
    pub async fn get_ledger_entries(
        &self,
        keys: &[LedgerKey],
        ledger_seq: Option<u32>,
    ) -> Result<GetLedgerEntriesResponse, ChainError> {
        let mut all_entries = Vec::with_capacity(keys.len());
        let mut latest_ledger = None;

        for chunk in keys.chunks(MAX_LEDGER_KEYS_PER_CALL) {
            let key_strings: Vec<&str> = chunk.iter().map(|k| k.key.as_str()).collect();
            let mut params = json!({ "keys": key_strings });
            if let Some(seq) = ledger_seq {
                params["ledgerSeq"] = json!(seq);
            }

            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getLedgerEntries",
                "params": params,
            });

            let response = self
                .http
                .post(&self.endpoint)
                .json(&body)
                .send()
                .await
                .map_err(|e| ChainError::Rpc(e.to_string()))?;

            let status = response.status();
            let value: Value = response
                .json()
                .await
                .map_err(|e| ChainError::Rpc(e.to_string()))?;

            if !status.is_success() {
                return Err(ChainError::Rpc(format!(
                    "getLedgerEntries failed with status {status}: {value}"
                )));
            }

            if let Some(err) = value.get("error") {
                return Err(ChainError::Rpc(err.to_string()));
            }

            let result = value.get("result").cloned().unwrap_or(Value::Null);
            let parsed: GetLedgerEntriesResponse = serde_json::from_value(result)
                .map_err(|e| ChainError::Rpc(e.to_string()))?;

            if latest_ledger.is_none() {
                latest_ledger = parsed.latest_ledger;
            }
            all_entries.extend(parsed.entries);
        }

        Ok(GetLedgerEntriesResponse {
            entries: all_entries,
            latest_ledger,
        })
    }
}
