//! Minimal Soroban JSON-RPC transport.
//!
//! The chain-safety modules only need a handful of read-only RPC methods
//! (`getVersionInfo`, `getNetwork`, `getLatestLedger`, `getLedgers`,
//! `getTransaction`). They talk to the network through the [`JsonRpc`] trait
//! so tests can script responses without a live node.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// Network / HTTP failure reaching the node.
    Transport(String),
    /// The node answered with a JSON-RPC `error` object.
    Node { code: i64, message: String },
    /// The node answered, but not in the shape we expected.
    Malformed(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "rpc transport error: {e}"),
            Self::Node { code, message } => write!(f, "rpc error {code}: {message}"),
            Self::Malformed(e) => write!(f, "malformed rpc response: {e}"),
        }
    }
}

impl std::error::Error for RpcError {}

#[async_trait]
pub trait JsonRpc: Send + Sync {
    /// Calls `method` with `params` and returns the `result` member.
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

/// JSON-RPC over HTTP using `reqwest`.
pub struct HttpJsonRpc {
    endpoint: String,
    http: reqwest::Client,
}

impl HttpJsonRpc {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[async_trait]
impl JsonRpc for HttpJsonRpc {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .map_err(|e| RpcError::Malformed(format!("HTTP {status}: {e}")))?;
        unwrap_envelope(value)
    }
}

/// Extracts `result` from a JSON-RPC response envelope, or maps `error`.
pub fn unwrap_envelope(value: Value) -> Result<Value, RpcError> {
    if let Some(err) = value.get("error") {
        return Err(RpcError::Node {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        });
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| RpcError::Malformed("missing `result`".into()))
}

/// Test double: returns queued responses per method, in order. The last
/// response for a method is repeated once its queue drains.
#[derive(Default)]
pub struct ScriptedJsonRpc {
    responses: Mutex<HashMap<String, VecDeque<Result<Value, RpcError>>>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl ScriptedJsonRpc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, method: &str, response: Result<Value, RpcError>) -> &Self {
        self.responses
            .lock()
            .unwrap()
            .entry(method.to_string())
            .or_default()
            .push_back(response);
        self
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl JsonRpc for ScriptedJsonRpc {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        let mut responses = self.responses.lock().unwrap();
        let queue = responses
            .get_mut(method)
            .ok_or_else(|| RpcError::Transport(format!("no scripted response for {method}")))?;
        if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue
                .front()
                .cloned()
                .ok_or_else(|| RpcError::Transport(format!("no scripted response for {method}")))?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_maps_error_and_result() {
        assert_eq!(unwrap_envelope(json!({"result": 5})).unwrap(), json!(5));
        assert_eq!(
            unwrap_envelope(json!({"error": {"code": -32601, "message": "nope"}})),
            Err(RpcError::Node {
                code: -32601,
                message: "nope".into()
            })
        );
        assert!(matches!(
            unwrap_envelope(json!({})),
            Err(RpcError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn scripted_rpc_replays_in_order_then_repeats_last() {
        let rpc = ScriptedJsonRpc::new();
        rpc.push("m", Ok(json!(1))).push("m", Ok(json!(2)));
        assert_eq!(rpc.call("m", json!({})).await.unwrap(), json!(1));
        assert_eq!(rpc.call("m", json!({})).await.unwrap(), json!(2));
        assert_eq!(rpc.call("m", json!({})).await.unwrap(), json!(2));
        assert!(rpc.call("other", json!({})).await.is_err());
    }
}
