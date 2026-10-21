//! Soroban protocol-version compatibility tracking and gating.
//!
//! Three version numbers have to agree before the backend may run in chain
//! mode or deploy a contract:
//!
//! * the protocol the **network** is closing ledgers at (`getLatestLedger`),
//! * the protocol the **RPC node** reports it was built for (`getVersionInfo`
//!   / `getNetwork`) — these differ while an RPC lags a network upgrade,
//! * the protocol range the **compiled contract SDK** supports. For a WASM
//!   about to be deployed this is read straight out of its
//!   `contractenvmetav0` custom section.
//!
//! [`ProtocolMonitor::check_once`] fetches the network side, evaluates it
//! against [`ContractSdkInfo`] and an optional operator-declared
//! [`ScheduledUpgrade`], persists the observation, raises
//! [`OperatorSignal`]s through a [`RunbookHook`], and stores the resulting
//! [`CompatibilityReport`] in a [`ProtocolGate`]. Chain-mode startup and
//! contract deployment call the gate and refuse to proceed unless a fresh,
//! verified report says it is safe. See `docs/runbooks/network-upgrade.md`.

use super::jsonrpc::{JsonRpc, RpcError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const RUNBOOK_PATH: &str = "docs/runbooks/network-upgrade.md";
const MIGRATION_SQL: &str = include_str!("../../migrations/0012_protocol_observations.sql");

/// Creates the tables this module writes to (idempotent).
pub async fn ensure_schema(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(MIGRATION_SQL).execute(db).await.map(|_| ())
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── SDK side ────────────────────────────────────────────────────────────────

/// What the settlement contracts were compiled against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSdkInfo {
    pub sdk_version: String,
    /// Lowest network protocol the compiled contracts can run on.
    pub min_protocol: u32,
    /// Highest network protocol the backend's decoding/encoding has been
    /// verified against. Running past this is a mismatch.
    pub max_protocol: u32,
}

impl ContractSdkInfo {
    /// Defaults to the SDK pinned for the settlement contracts; override with
    /// `ZENITH_CONTRACT_SDK_VERSION`, `ZENITH_CONTRACT_MIN_PROTOCOL` and
    /// `ZENITH_CONTRACT_MAX_PROTOCOL`.
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        Self {
            sdk_version: get("ZENITH_CONTRACT_SDK_VERSION").unwrap_or_else(|| "22.0.0".into()),
            min_protocol: get("ZENITH_CONTRACT_MIN_PROTOCOL")
                .and_then(|v| v.parse().ok())
                .unwrap_or(22),
            max_protocol: get("ZENITH_CONTRACT_MAX_PROTOCOL")
                .and_then(|v| v.parse().ok())
                .unwrap_or(23),
        }
    }

    pub fn supports(&self, protocol: u32) -> bool {
        (self.min_protocol..=self.max_protocol).contains(&protocol)
    }
}

/// Interface version embedded by `soroban-sdk` in a contract WASM's
/// `contractenvmetav0` custom section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WasmInterfaceVersion {
    pub protocol: u32,
    /// Non-zero for contracts built against a pre-release environment, which
    /// are only valid on networks running exactly that pre-release.
    pub pre_release: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmMetaError {
    NotWasm,
    Truncated,
    MissingEnvMeta,
}

impl std::fmt::Display for WasmMetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWasm => write!(f, "not a WASM module"),
            Self::Truncated => write!(f, "WASM module is truncated"),
            Self::MissingEnvMeta => write!(f, "WASM has no contractenvmetav0 section"),
        }
    }
}

fn read_leb_u32(buf: &[u8], pos: &mut usize) -> Result<u32, WasmMetaError> {
    let mut result: u32 = 0;
    for shift in (0..35).step_by(7) {
        let byte = *buf.get(*pos).ok_or(WasmMetaError::Truncated)?;
        *pos += 1;
        result |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(WasmMetaError::Truncated)
}

/// Reads the interface version out of a compiled contract.
///
/// The section holds XDR `SCEnvMetaEntry` values: a 4-byte kind
/// (`0` = interface version) followed by `uint32 protocol; uint32 preRelease`
/// (the same 8 bytes as the older single `uint64` encoding).
pub fn wasm_interface_version(wasm: &[u8]) -> Result<WasmInterfaceVersion, WasmMetaError> {
    if wasm.len() < 8 || &wasm[0..4] != b"\0asm" {
        return Err(WasmMetaError::NotWasm);
    }
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let size = read_leb_u32(wasm, &mut pos)? as usize;
        let end = pos.checked_add(size).ok_or(WasmMetaError::Truncated)?;
        if end > wasm.len() {
            return Err(WasmMetaError::Truncated);
        }
        if id == 0 {
            let mut p = pos;
            let name_len = read_leb_u32(wasm, &mut p)? as usize;
            let name_end = p + name_len;
            if name_end > end {
                return Err(WasmMetaError::Truncated);
            }
            if &wasm[p..name_end] == b"contractenvmetav0" {
                let mut q = name_end;
                while q + 12 <= end {
                    let kind = u32::from_be_bytes(wasm[q..q + 4].try_into().unwrap());
                    if kind == 0 {
                        return Ok(WasmInterfaceVersion {
                            protocol: u32::from_be_bytes(wasm[q + 4..q + 8].try_into().unwrap()),
                            pre_release: u32::from_be_bytes(
                                wasm[q + 8..q + 12].try_into().unwrap(),
                            ),
                        });
                    }
                    q += 12;
                }
            }
        }
        pos = end;
    }
    Err(WasmMetaError::MissingEnvMeta)
}

// ─── Network side ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkProtocolInfo {
    /// Protocol the RPC build targets (`getVersionInfo.protocolVersion`).
    pub rpc_protocol: u32,
    /// Protocol of the most recently closed ledger (`getLatestLedger`).
    pub ledger_protocol: u32,
    pub latest_ledger: u32,
    pub passphrase: String,
    pub rpc_version: Option<String>,
    pub core_version: Option<String>,
}

fn u32_field(v: &Value, field: &str, method: &str) -> Result<u32, RpcError> {
    v.get(field)
        .and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
        .and_then(|x| u32::try_from(x).ok())
        .ok_or_else(|| RpcError::Malformed(format!("{method}: missing `{field}`")))
}

pub async fn fetch_network_info(rpc: &dyn JsonRpc) -> Result<NetworkProtocolInfo, RpcError> {
    let version = rpc.call("getVersionInfo", json!({})).await?;
    let network = rpc.call("getNetwork", json!({})).await?;
    let latest = rpc.call("getLatestLedger", json!({})).await?;

    let str_field = |v: &Value, a: &str, b: &str| {
        v.get(a)
            .or_else(|| v.get(b))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    Ok(NetworkProtocolInfo {
        rpc_protocol: u32_field(&version, "protocolVersion", "getVersionInfo")
            .or_else(|_| u32_field(&network, "protocolVersion", "getNetwork"))?,
        ledger_protocol: u32_field(&latest, "protocolVersion", "getLatestLedger")?,
        latest_ledger: u32_field(&latest, "sequence", "getLatestLedger")?,
        passphrase: network
            .get("passphrase")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::Malformed("getNetwork: missing `passphrase`".into()))?
            .to_string(),
        rpc_version: str_field(&version, "version", "version"),
        core_version: str_field(&version, "captiveCoreVersion", "captive_core_version"),
    })
}

// ─── Evaluation ──────────────────────────────────────────────────────────────

/// A protocol upgrade the validators have announced. Soroban RPC does not
/// expose pending upgrades, so the operator declares it (see the runbook):
/// `ZENITH_SCHEDULED_UPGRADE=<protocol>@<unix-seconds>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledUpgrade {
    pub target_protocol: u32,
    pub scheduled_at: u64,
}

impl std::str::FromStr for ScheduledUpgrade {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (p, t) = s
            .split_once('@')
            .ok_or_else(|| format!("expected <protocol>@<unix-seconds>, got {s:?}"))?;
        Ok(Self {
            target_protocol: p.trim().parse().map_err(|e| format!("protocol: {e}"))?,
            scheduled_at: t.trim().parse().map_err(|e| format!("timestamp: {e}"))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolPolicy {
    pub expected_passphrase: Option<String>,
    pub scheduled_upgrade: Option<ScheduledUpgrade>,
    /// Deployments are frozen this long before a scheduled upgrade the SDK
    /// does not support, so nothing new lands right before a breaking change.
    pub deploy_freeze: Duration,
    /// A report older than this is not trusted by the gate.
    pub max_report_age: Duration,
}

impl Default for ProtocolPolicy {
    fn default() -> Self {
        Self {
            expected_passphrase: None,
            scheduled_upgrade: None,
            deploy_freeze: Duration::from_secs(24 * 3600),
            max_report_age: Duration::from_secs(10 * 60),
        }
    }
}

impl ProtocolPolicy {
    /// Reads `ZENITH_EXPECTED_NETWORK_PASSPHRASE`, `ZENITH_SCHEDULED_UPGRADE`,
    /// `ZENITH_UPGRADE_DEPLOY_FREEZE_SECS` and `ZENITH_PROTOCOL_REPORT_MAX_AGE_SECS`.
    pub fn from_env() -> Result<Self, String> {
        let mut p = Self {
            expected_passphrase: std::env::var("ZENITH_EXPECTED_NETWORK_PASSPHRASE").ok(),
            ..Self::default()
        };
        if let Ok(v) = std::env::var("ZENITH_SCHEDULED_UPGRADE") {
            if !v.trim().is_empty() {
                p.scheduled_upgrade = Some(v.parse()?);
            }
        }
        if let Some(s) = std::env::var("ZENITH_UPGRADE_DEPLOY_FREEZE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            p.deploy_freeze = Duration::from_secs(s);
        }
        if let Some(s) = std::env::var("ZENITH_PROTOCOL_REPORT_MAX_AGE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            p.max_report_age = Duration::from_secs(s);
        }
        Ok(p)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    /// Safe to keep running, but deployments are blocked.
    BlocksDeploy,
    /// Neither chain-mode startup nor deployment may proceed.
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub code: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityStatus {
    Compatible,
    /// Running is fine, deploying is not.
    DeployBlocked,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityReport {
    pub status: CompatibilityStatus,
    pub network: NetworkProtocolInfo,
    pub sdk: ContractSdkInfo,
    pub scheduled_upgrade: Option<ScheduledUpgrade>,
    pub findings: Vec<Finding>,
    pub checked_at: u64,
}

impl CompatibilityReport {
    pub fn worst(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }
}

/// Pure evaluation — no I/O, so every rule is unit-testable.
pub fn evaluate(
    network: &NetworkProtocolInfo,
    sdk: &ContractSdkInfo,
    policy: &ProtocolPolicy,
    now: u64,
) -> CompatibilityReport {
    let mut findings = Vec::new();
    let mut push = |code: &str, severity, message: String| {
        findings.push(Finding {
            code: code.into(),
            severity,
            message,
        })
    };

    if let Some(expected) = &policy.expected_passphrase {
        if expected != &network.passphrase {
            push(
                "network_passphrase_mismatch",
                Severity::Fatal,
                format!(
                    "RPC is on {:?}, expected {:?}",
                    network.passphrase, expected
                ),
            );
        }
    }

    if network.rpc_protocol != network.ledger_protocol {
        push(
            "rpc_ledger_protocol_mismatch",
            Severity::Fatal,
            format!(
                "RPC build targets protocol {} but the network is closing ledgers at protocol {}; \
                 the RPC must be upgraded/rolled back before it can be trusted",
                network.rpc_protocol, network.ledger_protocol
            ),
        );
    }

    let p = network.ledger_protocol;
    if p < sdk.min_protocol {
        push(
            "network_protocol_too_old",
            Severity::Fatal,
            format!(
                "network is on protocol {p}; contracts built with soroban-sdk {} need >= {}",
                sdk.sdk_version, sdk.min_protocol
            ),
        );
    } else if p > sdk.max_protocol {
        push(
            "network_protocol_unsupported",
            Severity::Fatal,
            format!(
                "network is on protocol {p}; soroban-sdk {} is only verified up to {}",
                sdk.sdk_version, sdk.max_protocol
            ),
        );
    }

    if let Some(up) = policy.scheduled_upgrade {
        if up.target_protocol > p {
            let supported = sdk.supports(up.target_protocol);
            let secs_left = up.scheduled_at.saturating_sub(now);
            if now >= up.scheduled_at {
                push(
                    "scheduled_upgrade_overdue",
                    if supported { Severity::Info } else { Severity::BlocksDeploy },
                    format!(
                        "upgrade to protocol {} was scheduled {}s ago but the network is still on {p}",
                        up.target_protocol,
                        now - up.scheduled_at
                    ),
                );
            } else if !supported && Duration::from_secs(secs_left) <= policy.deploy_freeze {
                push(
                    "unsupported_upgrade_imminent",
                    Severity::BlocksDeploy,
                    format!(
                        "network upgrades to protocol {} in {secs_left}s and soroban-sdk {} does not \
                         support it; deployments are frozen",
                        up.target_protocol, sdk.sdk_version
                    ),
                );
            } else {
                push(
                    "upgrade_scheduled",
                    Severity::Info,
                    format!(
                        "network upgrades to protocol {} in {secs_left}s (supported by SDK: {supported})",
                        up.target_protocol
                    ),
                );
            }
        }
    }

    let status = match findings.iter().map(|f| f.severity).max() {
        Some(Severity::Fatal) => CompatibilityStatus::Incompatible,
        Some(Severity::BlocksDeploy) => CompatibilityStatus::DeployBlocked,
        _ => CompatibilityStatus::Compatible,
    };

    CompatibilityReport {
        status,
        network: network.clone(),
        sdk: sdk.clone(),
        scheduled_upgrade: policy.scheduled_upgrade,
        findings,
        checked_at: now,
    }
}

// ─── Gate ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// No compatibility check has completed yet.
    Unverified,
    /// The last successful check is too old to trust.
    Stale { age_secs: u64 },
    /// The last check found blocking problems.
    Blocked {
        status: CompatibilityStatus,
        findings: Vec<Finding>,
    },
    /// The WASM being deployed targets a protocol the network can't run.
    WasmIncompatible(String),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unverified => write!(f, "protocol compatibility has not been verified"),
            Self::Stale { age_secs } => {
                write!(f, "protocol compatibility check is stale ({age_secs}s old)")
            }
            Self::Blocked { status, findings } => {
                let codes: Vec<_> = findings.iter().map(|f| f.code.as_str()).collect();
                write!(f, "protocol gate is {status:?}: {}", codes.join(", "))
            }
            Self::WasmIncompatible(m) => write!(f, "contract WASM incompatible: {m}"),
        }
    }
}

impl std::error::Error for GateError {}

/// Holds the most recent [`CompatibilityReport`] and answers "may we
/// start / deploy?". Cheap to clone and share.
#[derive(Clone)]
pub struct ProtocolGate {
    report: Arc<RwLock<Option<CompatibilityReport>>>,
    max_age: Duration,
}

impl ProtocolGate {
    pub fn new(max_age: Duration) -> Self {
        Self {
            report: Arc::new(RwLock::new(None)),
            max_age,
        }
    }

    pub fn set(&self, report: CompatibilityReport) {
        *self.report.write().unwrap() = Some(report);
    }

    /// Clears the verified state, e.g. after the RPC became unreachable.
    pub fn invalidate(&self) {
        *self.report.write().unwrap() = None;
    }

    pub fn current(&self) -> Option<CompatibilityReport> {
        self.report.read().unwrap().clone()
    }

    fn fresh_report(&self, now: u64) -> Result<CompatibilityReport, GateError> {
        let report = self.current().ok_or(GateError::Unverified)?;
        let age = now.saturating_sub(report.checked_at);
        if Duration::from_secs(age) > self.max_age {
            return Err(GateError::Stale { age_secs: age });
        }
        Ok(report)
    }

    fn blocking(report: &CompatibilityReport, at_least: Severity) -> Vec<Finding> {
        report
            .findings
            .iter()
            .filter(|f| f.severity >= at_least)
            .cloned()
            .collect()
    }

    /// Chain mode may start unless something is fatally incompatible.
    pub fn check_chain_mode_startup(&self, now: u64) -> Result<CompatibilityReport, GateError> {
        let report = self.fresh_report(now)?;
        if report.status == CompatibilityStatus::Incompatible {
            return Err(GateError::Blocked {
                status: report.status,
                findings: Self::blocking(&report, Severity::Fatal),
            });
        }
        Ok(report)
    }

    /// Deployment needs a fully compatible report *and* a WASM whose
    /// embedded interface version the current network can execute.
    pub fn check_deploy(&self, wasm: &[u8], now: u64) -> Result<CompatibilityReport, GateError> {
        let report = self.fresh_report(now)?;
        if report.status != CompatibilityStatus::Compatible {
            return Err(GateError::Blocked {
                status: report.status,
                findings: Self::blocking(&report, Severity::BlocksDeploy),
            });
        }
        let iv =
            wasm_interface_version(wasm).map_err(|e| GateError::WasmIncompatible(e.to_string()))?;
        let net = report.network.ledger_protocol;
        if iv.protocol > net {
            return Err(GateError::WasmIncompatible(format!(
                "WASM built for protocol {} but network is on {net}",
                iv.protocol
            )));
        }
        if iv.pre_release != 0 {
            return Err(GateError::WasmIncompatible(format!(
                "WASM built against pre-release environment {}.{} cannot be deployed",
                iv.protocol, iv.pre_release
            )));
        }
        if !report.sdk.supports(iv.protocol) {
            return Err(GateError::WasmIncompatible(format!(
                "WASM interface protocol {} is outside the verified SDK range {}..={}",
                iv.protocol, report.sdk.min_protocol, report.sdk.max_protocol
            )));
        }
        Ok(report)
    }
}

// ─── Operator signals ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum OperatorSignal {
    /// The network's protocol changed since the last observation.
    ProtocolChanged { from: u32, to: u32 },
    /// An upgrade is declared; emitted on every check until it lands.
    UpgradeScheduled {
        target_protocol: u32,
        scheduled_at: u64,
        sdk_supports_target: bool,
    },
    /// The check found problems; the gate is closed for startup and/or deploys.
    CompatibilityDegraded {
        status: CompatibilityStatus,
        findings: Vec<Finding>,
    },
    /// Compatibility was restored after a degraded check.
    CompatibilityRestored,
    /// The check could not reach the RPC; the gate has been invalidated.
    RpcUnavailable { error: String },
}

impl OperatorSignal {
    pub fn runbook(&self) -> String {
        let anchor = match self {
            Self::ProtocolChanged { .. } => "after-the-upgrade",
            Self::UpgradeScheduled { .. } => "before-the-upgrade",
            Self::CompatibilityDegraded { .. } | Self::CompatibilityRestored => "finding-codes",
            Self::RpcUnavailable { .. } => "rpc-unavailable",
        };
        format!("{RUNBOOK_PATH}#{anchor}")
    }
}

/// Where operator signals go. Implement this to page, post to chat, or
/// trigger automation; the payload always carries the runbook link.
#[async_trait]
pub trait RunbookHook: Send + Sync {
    async fn notify(&self, signal: &OperatorSignal);
}

/// Default hook: structured log lines operators can alert on
/// (`target = "zenith::protocol"`).
pub struct LogHook;

#[async_trait]
impl RunbookHook for LogHook {
    async fn notify(&self, signal: &OperatorSignal) {
        let payload = serde_json::to_string(signal).unwrap_or_default();
        let runbook = signal.runbook();
        match signal {
            OperatorSignal::UpgradeScheduled { .. } | OperatorSignal::CompatibilityRestored => {
                tracing::info!(target: "zenith::protocol", %payload, %runbook, "protocol signal")
            }
            OperatorSignal::CompatibilityDegraded { .. }
            | OperatorSignal::RpcUnavailable { .. } => {
                tracing::error!(target: "zenith::protocol", %payload, %runbook, "protocol signal")
            }
            OperatorSignal::ProtocolChanged { .. } => {
                tracing::warn!(target: "zenith::protocol", %payload, %runbook, "protocol signal")
            }
        }
    }
}

/// POSTs `{ "signal": ..., "runbook": ... }` to `ZENITH_PROTOCOL_WEBHOOK_URL`.
/// Failures are logged and never block the check.
pub struct WebhookHook {
    url: String,
    http: reqwest::Client,
}

impl WebhookHook {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var("ZENITH_PROTOCOL_WEBHOOK_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .map(Self::new)
    }
}

#[async_trait]
impl RunbookHook for WebhookHook {
    async fn notify(&self, signal: &OperatorSignal) {
        let mut body = serde_json::to_value(signal).unwrap_or(Value::Null);
        body["runbook"] = json!(signal.runbook());
        if let Err(e) = self.http.post(&self.url).json(&body).send().await {
            tracing::warn!(target: "zenith::protocol", error = %e, "protocol webhook failed");
        }
    }
}

/// Fans a signal out to several hooks.
pub struct HookSet(pub Vec<Arc<dyn RunbookHook>>);

#[async_trait]
impl RunbookHook for HookSet {
    async fn notify(&self, signal: &OperatorSignal) {
        for h in &self.0 {
            h.notify(signal).await;
        }
    }
}

// ─── Monitor ─────────────────────────────────────────────────────────────────

pub struct ProtocolMonitor {
    rpc: Arc<dyn JsonRpc>,
    sdk: ContractSdkInfo,
    policy: ProtocolPolicy,
    gate: ProtocolGate,
    hook: Arc<dyn RunbookHook>,
    db: Option<SqlitePool>,
    last: std::sync::Mutex<Option<CompatibilityReport>>,
}

impl ProtocolMonitor {
    pub fn new(
        rpc: Arc<dyn JsonRpc>,
        sdk: ContractSdkInfo,
        policy: ProtocolPolicy,
        hook: Arc<dyn RunbookHook>,
        db: Option<SqlitePool>,
    ) -> Self {
        let gate = ProtocolGate::new(policy.max_report_age);
        Self {
            rpc,
            sdk,
            policy,
            gate,
            hook,
            db,
            last: std::sync::Mutex::new(None),
        }
    }

    pub fn gate(&self) -> ProtocolGate {
        self.gate.clone()
    }

    /// Runs one check, updates the gate, persists the observation and emits
    /// signals for anything operators need to act on.
    pub async fn check_once(&self, now: u64) -> Result<CompatibilityReport, RpcError> {
        let network = match fetch_network_info(self.rpc.as_ref()).await {
            Ok(n) => n,
            Err(e) => {
                self.gate.invalidate();
                self.hook
                    .notify(&OperatorSignal::RpcUnavailable {
                        error: e.to_string(),
                    })
                    .await;
                return Err(e);
            }
        };

        let report = evaluate(&network, &self.sdk, &self.policy, now);
        let previous = self.last.lock().unwrap().replace(report.clone());

        if let Some(prev) = &previous {
            if prev.network.ledger_protocol != report.network.ledger_protocol {
                self.hook
                    .notify(&OperatorSignal::ProtocolChanged {
                        from: prev.network.ledger_protocol,
                        to: report.network.ledger_protocol,
                    })
                    .await;
            }
        }
        if let Some(up) = report
            .scheduled_upgrade
            .filter(|u| u.target_protocol > network.ledger_protocol)
        {
            self.hook
                .notify(&OperatorSignal::UpgradeScheduled {
                    target_protocol: up.target_protocol,
                    scheduled_at: up.scheduled_at,
                    sdk_supports_target: self.sdk.supports(up.target_protocol),
                })
                .await;
        }
        let was_ok = previous
            .as_ref()
            .map(|p| p.status == CompatibilityStatus::Compatible);
        if report.status != CompatibilityStatus::Compatible {
            self.hook
                .notify(&OperatorSignal::CompatibilityDegraded {
                    status: report.status,
                    findings: report.findings.clone(),
                })
                .await;
        } else if was_ok == Some(false) {
            self.hook
                .notify(&OperatorSignal::CompatibilityRestored)
                .await;
        }

        if let Some(db) = &self.db {
            if let Err(e) = persist_observation(db, &report).await {
                tracing::warn!(target: "zenith::protocol", error = %e, "failed to persist protocol observation");
            }
        }

        self.gate.set(report.clone());
        Ok(report)
    }

    /// Checks on startup and refuses to return unless chain mode is allowed.
    pub async fn verify_chain_mode_startup(&self) -> Result<CompatibilityReport, StartupError> {
        let now = unix_now();
        self.check_once(now).await.map_err(StartupError::Rpc)?;
        self.gate
            .check_chain_mode_startup(now)
            .map_err(StartupError::Gate)
    }

    /// Re-checks every `interval` for the lifetime of the process.
    pub async fn run(self: Arc<Self>, interval: Duration) {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            let _ = self.check_once(unix_now()).await;
        }
    }
}

#[derive(Debug)]
pub enum StartupError {
    Rpc(RpcError),
    Gate(GateError),
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "chain mode refused: {e}"),
            Self::Gate(e) => write!(f, "chain mode refused: {e}"),
        }
    }
}

impl std::error::Error for StartupError {}

async fn persist_observation(db: &SqlitePool, r: &CompatibilityReport) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO protocol_observations
            (id, observed_at, rpc_protocol, ledger_protocol, latest_ledger, passphrase,
             rpc_version, core_version, sdk_version, sdk_min_protocol, sdk_max_protocol,
             status, findings)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(r.checked_at as i64)
    .bind(r.network.rpc_protocol as i64)
    .bind(r.network.ledger_protocol as i64)
    .bind(r.network.latest_ledger as i64)
    .bind(&r.network.passphrase)
    .bind(&r.network.rpc_version)
    .bind(&r.network.core_version)
    .bind(&r.sdk.sdk_version)
    .bind(r.sdk.min_protocol as i64)
    .bind(r.sdk.max_protocol as i64)
    .bind(
        serde_json::to_string(&r.status)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string(),
    )
    .bind(serde_json::to_string(&r.findings).unwrap_or_default())
    .execute(db)
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk() -> ContractSdkInfo {
        ContractSdkInfo {
            sdk_version: "22.0.0".into(),
            min_protocol: 22,
            max_protocol: 23,
        }
    }

    fn net(p: u32) -> NetworkProtocolInfo {
        NetworkProtocolInfo {
            rpc_protocol: p,
            ledger_protocol: p,
            latest_ledger: 100,
            passphrase: "Test SDF Network ; September 2015".into(),
            rpc_version: None,
            core_version: None,
        }
    }

    /// Minimal module: header + custom section `contractenvmetav0`.
    pub(crate) fn wasm_with_meta(protocol: u32, pre: u32) -> Vec<u8> {
        let name = b"contractenvmetav0";
        let mut payload = vec![name.len() as u8];
        payload.extend_from_slice(name);
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&protocol.to_be_bytes());
        payload.extend_from_slice(&pre.to_be_bytes());
        let mut w = b"\0asm\x01\0\0\0".to_vec();
        // an empty type section first, to exercise skipping
        w.extend_from_slice(&[1, 1, 0]);
        w.push(0);
        w.push(payload.len() as u8);
        w.extend(payload);
        w
    }

    #[test]
    fn parses_interface_version_from_wasm() {
        let v = wasm_interface_version(&wasm_with_meta(22, 0)).unwrap();
        assert_eq!(
            v,
            WasmInterfaceVersion {
                protocol: 22,
                pre_release: 0
            }
        );
        assert_eq!(wasm_interface_version(b"nope"), Err(WasmMetaError::NotWasm));
        assert_eq!(
            wasm_interface_version(b"\0asm\x01\0\0\0"),
            Err(WasmMetaError::MissingEnvMeta)
        );
        let mut truncated = wasm_with_meta(22, 0);
        truncated.truncate(truncated.len() - 3);
        assert_eq!(
            wasm_interface_version(&truncated),
            Err(WasmMetaError::Truncated)
        );
    }

    #[test]
    fn compatible_when_network_in_sdk_range() {
        let r = evaluate(&net(22), &sdk(), &ProtocolPolicy::default(), 0);
        assert_eq!(r.status, CompatibilityStatus::Compatible);
        assert!(r.findings.is_empty());
    }

    #[test]
    fn mismatches_are_fatal() {
        let p = ProtocolPolicy::default();
        assert_eq!(
            evaluate(&net(21), &sdk(), &p, 0).findings[0].code,
            "network_protocol_too_old"
        );
        assert_eq!(
            evaluate(&net(24), &sdk(), &p, 0).findings[0].code,
            "network_protocol_unsupported"
        );

        let mut lagging = net(22);
        lagging.ledger_protocol = 23;
        let r = evaluate(&lagging, &sdk(), &p, 0);
        assert_eq!(r.status, CompatibilityStatus::Incompatible);
        assert_eq!(r.findings[0].code, "rpc_ledger_protocol_mismatch");

        let p = ProtocolPolicy {
            expected_passphrase: Some("Public Global Stellar Network ; September 2015".into()),
            ..p
        };
        assert_eq!(
            evaluate(&net(22), &sdk(), &p, 0).findings[0].code,
            "network_passphrase_mismatch"
        );
    }

    #[test]
    fn unsupported_upgrade_freezes_deploys_inside_window_only() {
        let up = ScheduledUpgrade {
            target_protocol: 24,
            scheduled_at: 100_000,
        };
        let p = ProtocolPolicy {
            scheduled_upgrade: Some(up),
            ..Default::default()
        };

        let far = evaluate(&net(23), &sdk(), &p, 0);
        assert_eq!(far.status, CompatibilityStatus::Compatible);
        assert_eq!(far.findings[0].code, "upgrade_scheduled");

        let near = evaluate(&net(23), &sdk(), &p, 100_000 - 3600);
        assert_eq!(near.status, CompatibilityStatus::DeployBlocked);
        assert_eq!(near.findings[0].code, "unsupported_upgrade_imminent");

        let overdue = evaluate(&net(23), &sdk(), &p, 100_500);
        assert_eq!(overdue.findings[0].code, "scheduled_upgrade_overdue");

        // A supported target never freezes deploys.
        let ok = ProtocolPolicy {
            scheduled_upgrade: Some(ScheduledUpgrade {
                target_protocol: 23,
                scheduled_at: 100_000,
            }),
            ..Default::default()
        };
        assert_eq!(
            evaluate(&net(22), &sdk(), &ok, 99_999).status,
            CompatibilityStatus::Compatible
        );
    }

    #[test]
    fn scheduled_upgrade_parses() {
        assert_eq!(
            "24@1793984400".parse::<ScheduledUpgrade>().unwrap(),
            ScheduledUpgrade {
                target_protocol: 24,
                scheduled_at: 1793984400
            }
        );
        assert!("24".parse::<ScheduledUpgrade>().is_err());
    }

    #[test]
    fn gate_requires_fresh_verified_report() {
        let gate = ProtocolGate::new(Duration::from_secs(60));
        assert_eq!(
            gate.check_chain_mode_startup(0).unwrap_err(),
            GateError::Unverified
        );

        gate.set(evaluate(&net(22), &sdk(), &ProtocolPolicy::default(), 1000));
        assert!(gate.check_chain_mode_startup(1030).is_ok());
        assert!(matches!(
            gate.check_chain_mode_startup(1100),
            Err(GateError::Stale { .. })
        ));

        gate.set(evaluate(&net(24), &sdk(), &ProtocolPolicy::default(), 1000));
        assert!(matches!(
            gate.check_chain_mode_startup(1000),
            Err(GateError::Blocked { .. })
        ));
    }

    #[test]
    fn deploy_gate_checks_wasm_against_network() {
        let gate = ProtocolGate::new(Duration::from_secs(60));
        gate.set(evaluate(&net(22), &sdk(), &ProtocolPolicy::default(), 0));
        assert!(gate.check_deploy(&wasm_with_meta(22, 0), 0).is_ok());
        assert!(matches!(
            gate.check_deploy(&wasm_with_meta(23, 0), 0),
            Err(GateError::WasmIncompatible(_))
        ));
        assert!(matches!(
            gate.check_deploy(&wasm_with_meta(22, 1), 0),
            Err(GateError::WasmIncompatible(_))
        ));
        assert!(matches!(
            gate.check_deploy(b"junk", 0),
            Err(GateError::WasmIncompatible(_))
        ));

        // Startup allowed but deploy frozen ahead of an unsupported upgrade.
        let p = ProtocolPolicy {
            scheduled_upgrade: Some(ScheduledUpgrade {
                target_protocol: 24,
                scheduled_at: 10,
            }),
            ..Default::default()
        };
        gate.set(evaluate(&net(22), &sdk(), &p, 0));
        assert!(gate.check_chain_mode_startup(0).is_ok());
        assert!(matches!(
            gate.check_deploy(&wasm_with_meta(22, 0), 0),
            Err(GateError::Blocked { .. })
        ));
    }
}
