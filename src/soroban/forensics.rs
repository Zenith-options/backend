//! Forensics for failed Soroban invocations.
//!
//! When a simulation, submission or confirmed transaction fails, the RPC
//! response carries the host's diagnostic events: the `fn_call` / `fn_return`
//! trace, `error` events raised by the host or a contract, and `log` lines.
//! [`analyze`] decodes those (requested with `xdrFormat: "json"`) together with
//! the transaction / operation result codes and return value into a
//! [`ForensicsRecord`]: who called what, which frame trapped, and with which
//! error. [`ForensicsStore`] persists it against the transaction attempt and
//! [`router`] serves it to operators, so a failed settlement can be diagnosed
//! from the record alone without replaying it against the network.
//!
//! Base64 XDR is always kept verbatim alongside the decoded view; if a node
//! only returns XDR the record is still written, flagged `decoded = false`.

use super::jsonrpc::{JsonRpc, RpcError};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

const MIGRATION_SQL: &str = include_str!("../../migrations/0014_tx_forensics.sql");

pub async fn ensure_schema(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(MIGRATION_SQL).execute(db).await.map(|_| ())
}

/// Zenith `#[contracterror]` codes (mirrors `chain::errors::ZENITH_CONTRACT_ERRORS`).
pub fn contract_error_name(code: u32) -> Option<&'static str> {
    Some(match code {
        1 => "InsufficientCollateral",
        2 => "SeriesExpired",
        3 => "SeriesNotFound",
        4 => "MissingTrustline",
        5 => "Unauthorized",
        _ => return None,
    })
}

// ─── Model ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Simulate,
    Submit,
    Confirm,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Simulate => "simulate",
            Self::Submit => "submit",
            Self::Confirm => "confirm",
        }
    }
}

/// Identifies the transaction attempt a record belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptContext {
    pub attempt_id: String,
    pub tx_hash: Option<String>,
    pub stage: Stage,
    /// Contract / function the backend meant to invoke, if known.
    pub contract_id: Option<String>,
    pub function: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostError {
    /// `contract`, `wasm_vm`, `context`, `storage`, `object`, `crypto`,
    /// `events`, `budget`, `value` or `auth`.
    pub kind: String,
    /// Numeric code for `contract`, otherwise the `ScErrorCode` name.
    pub code: String,
}

impl HostError {
    pub fn contract_code(&self) -> Option<u32> {
        (self.kind == "contract")
            .then(|| self.code.parse().ok())
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub contract_id: Option<String>,
    pub in_successful_contract_call: bool,
    /// `fn_call`, `fn_return`, `error`, `log`, `core_metrics`, or `contract`
    /// for events a contract emitted itself.
    pub kind: String,
    pub topics: Vec<Value>,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallFrame {
    pub depth: u32,
    pub contract_id: Option<String>,
    pub function: Option<String>,
    pub args: Value,
    /// `false` for frames that never returned — the trapped call path.
    pub returned: bool,
    pub return_value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForensicsRecord {
    pub id: String,
    pub attempt: AttemptContext,
    pub rpc_status: Option<String>,
    pub ledger: Option<u32>,
    /// Transaction result code, e.g. `tx_failed`.
    pub tx_result: Option<String>,
    /// `InvokeHostFunction` result code, e.g. `trapped`, `resource_limit_exceeded`.
    pub op_result: Option<String>,
    pub host_error: Option<HostError>,
    pub contract_error_code: Option<u32>,
    pub contract_error_name: Option<String>,
    pub error_message: Option<String>,
    pub return_value: Option<Value>,
    pub call_trace: Vec<CallFrame>,
    /// Innermost frame that never returned.
    pub trapped_frame: Option<CallFrame>,
    pub logs: Vec<String>,
    pub diagnostic_events: Vec<DiagnosticEvent>,
    pub diagnostic_events_xdr: Vec<String>,
    pub result_xdr: Option<String>,
    /// Whether diagnostic events were available in decoded (JSON) form.
    pub decoded: bool,
    pub summary: String,
    pub captured_at: u64,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// 32-byte contract hash (hex) → `C...` strkey. Strkeys pass through.
pub fn contract_strkey(id: &str) -> String {
    if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(bytes) = data_encoding::HEXLOWER_PERMISSIVE.decode(id.as_bytes()) {
            let mut payload = vec![2 << 3];
            payload.extend_from_slice(&bytes);
            let crc = crc16_xmodem(&payload);
            payload.extend_from_slice(&crc.to_le_bytes());
            return data_encoding::BASE32_NOPAD.encode(&payload);
        }
    }
    id.to_string()
}

fn symbol(v: &Value) -> Option<&str> {
    v.get("symbol")
        .and_then(Value::as_str)
        .or_else(|| v.as_str())
}

/// Renders a tagged ScVal (`{"string": "x"}` etc.) as display text.
fn scval_text(v: &Value) -> String {
    match v {
        Value::Object(m) if m.len() == 1 => {
            let (k, inner) = m.iter().next().unwrap();
            match (k.as_str(), inner) {
                ("vec", Value::Array(items)) => {
                    items.iter().map(scval_text).collect::<Vec<_>>().join(", ")
                }
                (_, Value::String(s)) => s.clone(),
                (_, other) => other.to_string(),
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_sc_error(v: &Value) -> Option<HostError> {
    let err = v.get("error")?.as_object()?;
    let (kind, code) = err.iter().next()?;
    Some(HostError {
        kind: kind.clone(),
        code: match code {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        },
    })
}

/// Parses the host's textual form, e.g. `Error(Contract, #3)` or
/// `Error(WasmVm, InvalidAction)`.
pub fn parse_error_text(s: &str) -> Option<HostError> {
    let start = s.find("Error(")? + "Error(".len();
    let end = start + s[start..].find(')')?;
    let (kind, code) = s[start..end].split_once(',')?;
    let snake = |t: &str| {
        let mut out = String::new();
        for (i, c) in t.trim().chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        }
        out
    };
    let code = code.trim().trim_start_matches('#');
    Some(HostError {
        kind: snake(kind),
        code: if code.chars().all(|c| c.is_ascii_digit()) {
            code.to_string()
        } else {
            snake(code)
        },
    })
}

/// Depth-first search for the first value under `key`.
fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m
            .get(key)
            .or_else(|| m.values().find_map(|x| find_key(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

fn decode_event(v: &Value) -> Option<DiagnosticEvent> {
    // Accept both the DiagnosticEvent wrapper and a bare ContractEvent.
    let (in_success, ev) = match v.get("event") {
        Some(ev) => (
            v.get("in_successful_contract_call")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ev,
        ),
        None => (false, v),
    };
    let body = ev.get("body").map(|b| b.get("v0").unwrap_or(b))?;
    let topics = body.get("topics")?.as_array()?.clone();
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    let ty = ev
        .get("type_")
        .or_else(|| ev.get("type"))
        .and_then(Value::as_str);
    let first = topics.first().and_then(symbol);
    let kind = match (ty, first) {
        (Some("contract"), _) => "contract",
        (_, Some(k @ ("fn_call" | "fn_return" | "error" | "log" | "core_metrics"))) => k,
        _ => "contract",
    }
    .to_string();
    Some(DiagnosticEvent {
        contract_id: ev
            .get("contract_id")
            .and_then(Value::as_str)
            .map(contract_strkey),
        in_successful_contract_call: in_success,
        kind,
        topics,
        data,
    })
}

/// Rebuilds the call tree from `fn_call` / `fn_return` pairs.
fn call_trace(events: &[DiagnosticEvent]) -> Vec<CallFrame> {
    let mut frames: Vec<CallFrame> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for e in events {
        match e.kind.as_str() {
            "fn_call" => {
                let callee = e.topics.get(1).map(|t| {
                    let raw = t
                        .get("bytes")
                        .and_then(Value::as_str)
                        .or_else(|| t.as_str());
                    raw.map(contract_strkey).unwrap_or_else(|| t.to_string())
                });
                frames.push(CallFrame {
                    depth: stack.len() as u32,
                    contract_id: callee,
                    function: e.topics.get(2).and_then(symbol).map(str::to_string),
                    args: e.data.clone(),
                    returned: false,
                    return_value: None,
                });
                stack.push(frames.len() - 1);
            }
            "fn_return" => {
                if let Some(i) = stack.pop() {
                    frames[i].returned = true;
                    frames[i].return_value = Some(e.data.clone());
                }
            }
            _ => {}
        }
    }
    frames
}

// ─── Analysis ────────────────────────────────────────────────────────────────

/// Pulls diagnostic events out of a simulate / send / getTransaction result.
fn diagnostic_payload(resp: &Value) -> (Vec<Value>, Vec<String>) {
    let json_keys = ["diagnosticEventsJson", "eventsJson"];
    let xdr_keys = ["diagnosticEventsXdr", "events"];
    let lookup = |keys: &[&str]| -> Option<Vec<Value>> {
        keys.iter().find_map(|k| {
            resp.get(k)
                .or_else(|| resp.get("events").and_then(|e| e.get(*k)))
                .and_then(Value::as_array)
                .cloned()
        })
    };
    let json_events = lookup(&json_keys).unwrap_or_default();
    let xdr = lookup(&xdr_keys)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (json_events, xdr)
}

/// Returns `true` if this RPC result represents a failed invocation.
pub fn is_failure(stage: Stage, resp: &Value) -> bool {
    match stage {
        Stage::Simulate => resp.get("error").is_some(),
        Stage::Submit => resp.get("status").and_then(Value::as_str) == Some("ERROR"),
        Stage::Confirm => resp.get("status").and_then(Value::as_str) == Some("FAILED"),
    }
}

/// Decodes a failed RPC result into a forensics record. Returns `None` if
/// the response is not a failure.
pub fn analyze(ctx: &AttemptContext, resp: &Value, now: u64) -> Option<ForensicsRecord> {
    if !is_failure(ctx.stage, resp) {
        return None;
    }
    let (json_events, xdr) = diagnostic_payload(resp);
    let events: Vec<DiagnosticEvent> = json_events.iter().filter_map(decode_event).collect();
    let trace = call_trace(&events);
    let trapped_frame = trace
        .iter()
        .filter(|f| !f.returned)
        .max_by_key(|f| f.depth)
        .cloned();

    let error_events: Vec<&DiagnosticEvent> = events
        .iter()
        .filter(|e| e.kind == "error" && !e.in_successful_contract_call)
        .collect();
    // A contract-level error is the root cause; host errors raised while
    // unwinding (e.g. `WasmVm, InvalidAction`) are consequences of it.
    let host_error = error_events
        .iter()
        .filter_map(|e| e.topics.get(1).and_then(parse_sc_error))
        .min_by_key(|h| (h.kind != "contract") as u8)
        .or_else(|| {
            resp.get("error")
                .and_then(Value::as_str)
                .and_then(parse_error_text)
        });
    let error_message = error_events
        .iter()
        .map(|e| scval_text(&e.data))
        .find(|s| !s.is_empty())
        .or_else(|| {
            resp.get("error")
                .and_then(Value::as_str)
                .map(|s| s.lines().next().unwrap_or(s).to_string())
        });

    let logs = events
        .iter()
        .filter(|e| e.kind == "log")
        .map(|e| scval_text(&e.data))
        .collect();

    let result_json = resp
        .get("resultJson")
        .or_else(|| resp.get("errorResultJson"));
    let tx_result = result_json
        .and_then(|r| r.get("result"))
        .and_then(Value::as_object)
        .and_then(|m| m.keys().next().cloned());
    let op_result = result_json
        .and_then(|r| find_key(r, "invoke_host_function"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let return_value = resp
        .get("returnValueJson")
        .or_else(|| {
            resp.get("resultMetaJson")
                .and_then(|m| find_key(m, "return_value"))
        })
        .cloned();

    let contract_error_code = host_error.as_ref().and_then(HostError::contract_code);
    let contract_error_name = contract_error_code
        .and_then(contract_error_name)
        .map(str::to_string);

    let function = trapped_frame
        .as_ref()
        .and_then(|f| f.function.clone())
        .or_else(|| ctx.function.clone())
        .unwrap_or_else(|| "<unknown fn>".into());
    let contract = trapped_frame
        .as_ref()
        .and_then(|f| f.contract_id.clone())
        .or_else(|| ctx.contract_id.clone())
        .unwrap_or_else(|| "<unknown contract>".into());
    let mut summary = format!("{} failed: {function} on {contract}", ctx.stage.as_str());
    if let Some(h) = &host_error {
        summary.push_str(&format!(" — Error({}, {})", h.kind, h.code));
    }
    if let Some(n) = &contract_error_name {
        summary.push_str(&format!(" {n}"));
    }
    if let Some(op) = &op_result {
        summary.push_str(&format!(" [{op}]"));
    }

    Some(ForensicsRecord {
        id: uuid::Uuid::new_v4().to_string(),
        attempt: ctx.clone(),
        rpc_status: resp
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string),
        ledger: resp
            .get("ledger")
            .or_else(|| resp.get("latestLedger"))
            .and_then(Value::as_u64)
            .map(|l| l as u32),
        tx_result,
        op_result,
        host_error,
        contract_error_code,
        contract_error_name,
        error_message,
        return_value,
        call_trace: trace,
        trapped_frame,
        logs,
        decoded: !json_events.is_empty() || xdr.is_empty(),
        diagnostic_events: events,
        diagnostic_events_xdr: xdr,
        result_xdr: resp
            .get("resultXdr")
            .or_else(|| resp.get("errorResultXdr"))
            .and_then(Value::as_str)
            .map(str::to_string),
        summary,
        captured_at: now,
    })
}

// ─── Persistence ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForensicsSummary {
    pub id: String,
    pub attempt_id: String,
    pub tx_hash: Option<String>,
    pub stage: String,
    pub contract_error_code: Option<u32>,
    pub summary: String,
    pub captured_at: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ForensicsFilter {
    pub attempt_id: Option<String>,
    pub tx_hash: Option<String>,
    pub limit: Option<u32>,
}

/// `(id, attempt_id, tx_hash, stage, contract_error_code, summary, captured_at)`
type SummaryRow = (
    String,
    String,
    Option<String>,
    String,
    Option<i64>,
    String,
    i64,
);

#[derive(Clone)]
pub struct ForensicsStore {
    db: SqlitePool,
}

impl ForensicsStore {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn insert(&self, r: &ForensicsRecord) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO tx_forensics
                (id, attempt_id, tx_hash, stage, contract_id, function, rpc_status, ledger,
                 tx_result, op_result, host_error_kind, host_error_code, contract_error_code,
                 summary, record, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .bind(&r.id)
        .bind(&r.attempt.attempt_id)
        .bind(&r.attempt.tx_hash)
        .bind(r.attempt.stage.as_str())
        .bind(
            r.trapped_frame
                .as_ref()
                .and_then(|f| f.contract_id.clone())
                .or(r.attempt.contract_id.clone()),
        )
        .bind(
            r.trapped_frame
                .as_ref()
                .and_then(|f| f.function.clone())
                .or(r.attempt.function.clone()),
        )
        .bind(&r.rpc_status)
        .bind(r.ledger.map(i64::from))
        .bind(&r.tx_result)
        .bind(&r.op_result)
        .bind(r.host_error.as_ref().map(|h| h.kind.clone()))
        .bind(r.host_error.as_ref().map(|h| h.code.clone()))
        .bind(r.contract_error_code.map(i64::from))
        .bind(&r.summary)
        .bind(serde_json::to_string(r).unwrap_or_default())
        .bind(r.captured_at as i64)
        .execute(&self.db)
        .await
        .map(|_| ())
    }

    pub async fn get(&self, id: &str) -> Result<Option<ForensicsRecord>, sqlx::Error> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT record FROM tx_forensics WHERE id = ?1")
                .bind(id)
                .fetch_optional(&self.db)
                .await?;
        Ok(row.and_then(|(r,)| serde_json::from_str(&r).ok()))
    }

    pub async fn list(&self, f: &ForensicsFilter) -> Result<Vec<ForensicsSummary>, sqlx::Error> {
        let rows: Vec<SummaryRow> = sqlx::query_as(
            "SELECT id, attempt_id, tx_hash, stage, contract_error_code, summary, captured_at
                 FROM tx_forensics
                 WHERE (?1 IS NULL OR attempt_id = ?1) AND (?2 IS NULL OR tx_hash = ?2)
                 ORDER BY captured_at DESC, id
                 LIMIT ?3",
        )
        .bind(&f.attempt_id)
        .bind(&f.tx_hash)
        .bind(f.limit.unwrap_or(50).min(500) as i64)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, attempt_id, tx_hash, stage, code, summary, at)| ForensicsSummary {
                    id,
                    attempt_id,
                    tx_hash,
                    stage,
                    contract_error_code: code.map(|c| c as u32),
                    summary,
                    captured_at: at as u64,
                },
            )
            .collect())
    }

    /// Analyzes `resp` and, if it is a failure, persists and returns the record.
    pub async fn capture(
        &self,
        ctx: &AttemptContext,
        resp: &Value,
        now: u64,
    ) -> Result<Option<ForensicsRecord>, sqlx::Error> {
        let Some(record) = analyze(ctx, resp, now) else {
            return Ok(None);
        };
        self.insert(&record).await?;
        tracing::warn!(
            target: "zenith::forensics",
            forensics_id = %record.id,
            attempt_id = %ctx.attempt_id,
            tx_hash = ?ctx.tx_hash,
            "{}", record.summary
        );
        Ok(Some(record))
    }

    /// Fetches a submitted transaction with decoded diagnostics and records
    /// it if it failed.
    pub async fn capture_confirmed(
        &self,
        rpc: &dyn JsonRpc,
        ctx: &AttemptContext,
        now: u64,
    ) -> Result<Option<ForensicsRecord>, CaptureError> {
        let hash = ctx.tx_hash.as_deref().ok_or(CaptureError::MissingTxHash)?;
        let resp = rpc
            .call(
                "getTransaction",
                json!({ "hash": hash, "xdrFormat": "json" }),
            )
            .await
            .map_err(CaptureError::Rpc)?;
        let ctx = AttemptContext {
            stage: Stage::Confirm,
            ..ctx.clone()
        };
        self.capture(&ctx, &resp, now)
            .await
            .map_err(CaptureError::Db)
    }
}

#[derive(Debug)]
pub enum CaptureError {
    MissingTxHash,
    Rpc(RpcError),
    Db(sqlx::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTxHash => write!(f, "attempt has no tx hash"),
            Self::Rpc(e) => write!(f, "{e}"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

// ─── Operator API ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ApiState {
    store: ForensicsStore,
    admin_token: Option<Arc<str>>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let Some(expected) = &state.admin_token else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "forensics API disabled: ZENITH_ADMIN_TOKEN unset",
        ));
    };
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "invalid admin token"))
    }
}

async fn list_handler(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Query(filter): Query<ForensicsFilter>,
) -> Response {
    if let Err(r) = authorize(&s, &headers) {
        return r.into_response();
    }
    match s.store.list(&filter).await {
        Ok(rows) => Json(json!({ "records": rows })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_handler(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorize(&s, &headers) {
        return r.into_response();
    }
    match s.store.get(&id).await {
        Ok(Some(r)) => Json(r).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Operator endpoints, guarded by `Authorization: Bearer <admin_token>`:
///
/// * `GET /` — `?attempt_id=&tx_hash=&limit=` → `{ "records": [summary] }`
/// * `GET /:id` — full [`ForensicsRecord`]
///
/// Mount with `nest_service("/api/v1/admin/tx-forensics", router(..))`.
pub fn router(db: SqlitePool, admin_token: Option<String>) -> Router {
    Router::new()
        .route("/", get(list_handler))
        .route("/:id", get(get_handler))
        .with_state(ApiState {
            store: ForensicsStore::new(db),
            admin_token: admin_token.filter(|t| !t.is_empty()).map(Arc::from),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_hash_becomes_strkey() {
        let id = contract_strkey(&"00".repeat(32));
        assert_eq!(
            id,
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
        );
        assert_eq!(contract_strkey("CABC"), "CABC");
    }

    #[test]
    fn parses_host_error_text() {
        assert_eq!(
            parse_error_text("HostError: Error(Contract, #3)\n\nEvent log..."),
            Some(HostError {
                kind: "contract".into(),
                code: "3".into()
            })
        );
        assert_eq!(
            parse_error_text("Error(WasmVm, InvalidAction)"),
            Some(HostError {
                kind: "wasm_vm".into(),
                code: "invalid_action".into()
            })
        );
        assert_eq!(parse_error_text("nothing here"), None);
    }

    #[test]
    fn success_is_not_recorded() {
        let ctx = AttemptContext {
            attempt_id: "a".into(),
            tx_hash: None,
            stage: Stage::Confirm,
            contract_id: None,
            function: None,
        };
        assert!(analyze(&ctx, &json!({"status": "SUCCESS"}), 0).is_none());
        assert!(analyze(
            &AttemptContext {
                stage: Stage::Simulate,
                ..ctx
            },
            &json!({"results": []}),
            0
        )
        .is_none());
    }
}
