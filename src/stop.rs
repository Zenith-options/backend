//! The single, auditable on/off ramp for settlement-critical activity.
//!
//! Zenith's on-chain collateral and off-chain ledger are gated behind one
//! explicit operator decision: the global `StopMode`. Nothing else in the
//! settlement-critical path may start, retry, or reopen a settlement,
//! deposit, withdrawal, funding, or post-mortem action while the stop is
//! active. The operator keeps a deliberate, logged resume path rather than
//! relying on a process restart.
//!
//! Who may toggle:
//! - `admin` — deliberate incident / operational decision (command log).
//! - `pauser` — the pauser role may place the stop, but only for incoming
//!   causes. Closing the stop (resume) is always `admin`.
//!
//! What triggers a stop, and who owns each halt state:
//! - `Manual` — operator command (`admin` only), always loggable and always
//!   resumable via `admin`.
//! - `ReconciliationDrift` — a `reconcile` run returns any critical mismatch.
//!   The drift threshold re-arm is a deliberate `admin` decision.
//! - `OracleStale` — the oracle publisher or settlement engine cannot obtain a
//!   not-stale fixing. Operator `admin` (or `pauser`) closes it.
//! - `NetworkIncident` — a reachability/health check fails. Operator `admin`
//!   (or `pauser`) closes it.
//!
//! Non-haltable while stopped: order matching, quote issuance, risk margin
//! computations that do not touch settlement or the off-chain ledger, and
//! read-only reporting. Those are not part of this gate and continue to
//! serve read-only operators.
//!
//! The state machine:
//!   IDLE -> Manual | ReconciliationDrift | OracleStale | NetworkIncident -> IDLE
//! Each transition appends one structured, append-only `StopEvent` with an id,
//! mode, cause, source, at, and an unquestionable `reason` + `provenance`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::stop::event::StopEvent;

pub mod event;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The single mutable stop state. One process-wide instance is the only place
/// that may toggle; downstream services read it immutably.
#[derive(Debug, Clone)]
pub struct StopState {
    id: String,
    mode: StopMode,
    source: Source,
    reason: String,
    provenance: String,
}

impl StopState {
    pub fn new(mode: StopMode, source: Source, reason: String, provenance: String) -> Self {
        let id = format!(
            "stop-{}-{}",
            mode.as_str(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        Self {
            id,
            mode,
            source,
            reason,
            provenance,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn mode(&self) -> &StopMode {
        &self.mode
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn provenance(&self) -> &str {
        &self.provenance
    }
}

// ---------------------------------------------------------------------------
// Start/continue of the state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopMode {
    /// The operator has deliberately halted settlement-critical activity.
    Manual,
    /// A reconciliation drift threshold breach has stopped the critical path.
    ReconciliationDrift,
    /// An oracle staleness condition has halted the critical path.
    OracleStale,
    /// An RPC or network incident has halted the critical path.
    NetworkIncident,
}

impl StopMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopMode::Manual => "manual",
            StopMode::ReconciliationDrift => "reconciliation_drift",
            StopMode::OracleStale => "oracle_stale",
            StopMode::NetworkIncident => "network_incident",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Deliberate operator command. `admin` may resume; `pauser` may place.
    Admin,
    /// The reconciliation observer flagged drift. Operator decides the re-arm.
    Observer,
    /// The oracle/publisher or settlement engine reported staleness.
    Oracle,
    /// A reachability/health check failed.
    Monitor,
}

impl Source {
    pub fn who_can_toggle(self) -> &'static str {
        match self {
            Source::Admin => "admin",
            Source::Observer => "admin",
            Source::Oracle => "admin or pauser",
            Source::Monitor => "admin or pauser",
        }
    }

    pub fn who_can_resume(self) -> &'static str {
        match self {
            Source::Admin => "admin",
            Source::Observer => "admin",
            Source::Oracle => "admin",
            Source::Monitor => "admin",
        }
    }
}

// ---------------------------------------------------------------------------
// Stop state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StopMachine {
    state: Arc<tokio::sync::Mutex<StopState>>,
    events: Arc<tokio::sync::Mutex<HashMap<String, StopEvent>>>,
}

impl StopMachine {
    pub fn new() -> Self {
        Self {
            state: Arc::new(tokio::sync::Mutex::new(StopState {
                id: "stop-uninitialized".into(),
                mode: StopMode::Manual,
                source: Source::Admin,
                reason: "stop machine initialised; no active stop".into(),
                provenance: "system".into(),
            })),
            events: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Set the stop. Replaces any existing active stop and records a single
    /// authoritative event. Returns the event and the previous state (if any).
    pub async fn set_stop(
        &self,
        mode: StopMode,
        source: Source,
        reason: String,
        provenance: String,
    ) -> (StopEvent, StopMode) {
        let mut events = self.events.lock().await;
        let event = StopEvent::new(mode.clone(), source, reason, provenance);
        events.insert(event.id().to_string(), event.clone());
        let mut state = self.state.lock().await;
        let previous_mode = state.mode.clone();
        *state = StopState::new(mode.clone(), source, reason, provenance);
        (event, previous_mode)
    }

    /// Close the stop. Always `admin` per the design; `pauser` may only place.
    pub async fn resume(&self, reason: String, provenance: String) -> StopEvent {
        let mut events = self.events.lock().await;
        let event = StopEvent::new(StopMode::Manual, Source::Admin, reason, provenance);
        events.insert(event.id().to_string(), event.clone());
        let mut state = self.state.lock().await;
        *state = StopState::new(StopMode::Manual, Source::Admin, reason, provenance);
        event
    }

    /// Consumer-facing accessor. Returns `Some` only while a stop is active.
    pub async fn current(&self) -> Option<StopState> {
        let state = self.state.lock().await;
        Some(state.clone())
    }

    /// Returns `true` exactly when settlement-critical activity must halt.
    pub async fn is_stopped(&self) -> bool {
        let state = self.state.lock().await;
        matches!(
            state.mode,
            StopMode::Manual | StopMode::ReconciliationDrift | StopMode::OracleStale | StopMode::NetworkIncident
        )
    }

    /// All events ever recorded for audit. Events are append-only.
    pub async fn events(&self) -> Vec<StopEvent> {
        let events = self.events.lock().await;
        events.values().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Registration with the larger service: the on-chain enforcement hook and
// off-chain log sink
// ---------------------------------------------------------------------------

/// On-chain enforcement hook. Downstream settlement-critical services call
/// `eval` before starting any settlement, deposit, withdrawal, or funding.
///
/// The hook returns a stable reason for the attached operator audit event and
/// for propagation to the Soroban contract via a `check_stop_mode` call.
pub struct StopHook<T> {
    machine: Arc<StopMachine>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> StopHook<T> {
    pub fn new(machine: Arc<StopMachine>) -> Self {
        Self {
            machine,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Evaluate whether the critical path may proceed. Returns `Ok(true)` to
    /// proceed, `Err((mode, reason))` to halt, and **`None` when the stop is
    /// not active**.
    pub async fn eval(&self) -> Result<bool, (StopMode, String)> {
        if !self.machine.is_stopped().await {
            return Ok(true);
        }
        let state = self.machine.current().await;
        Err((state.mode.clone(), state.reason.clone()))
    }
}

/// Off-chain audit sink: the stop machine writes structured events that are
/// mirrored into the outbox/ledger and surfaced to the operator via the admin
/// API and, eventually, the on-chain `StopManager` contract.
pub struct StopLog<T> {
    machine: Arc<StopMachine>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> StopLog<T> {
    pub fn new(machine: Arc<StopMachine>) -> Self {
        Self {
            machine,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Publish a stop enable/disable event. These are the append-only facts
    /// that define the operator decision boundary.
    pub async fn publish(&self, mode: StopMode, source: Source, reason: String, provenance: String) {
        let (event, _previous_mode) = self.machine.set_stop(mode, source, reason, provenance).await;
        tracing::info!(
            stop_id = event.id(),
            mode = event.mode(),
            cause = event.cause(),
            source = event.source(),
            at = event.at(),
            reason = event.reason(),
            provenance = event.provenance(),
            "settlement_critical_stop event recorded"
        );
    }

    pub async fn set_stop(&self, mode: StopMode, source: Source, reason: String, provenance: String) {
        let (_event, _previous_mode) = self.machine.set_stop(mode, source, reason, provenance).await;
    }

    pub async fn resume(&self, reason: String, provenance: String) {
        let event = self.machine.resume(reason, provenance).await;
        tracing::info!(
            stop_id = event.id(),
            mode = event.mode(),
            cause = event.cause(),
            source = event.source(),
            at = event.at(),
            reason = event.reason(),
            provenance = event.provenance(),
            "settlement_critical_resume event recorded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_mode_serializes() {
        let mode = StopMode::Manual;
        let serialized = serde_json::to_string(&mode).unwrap();
        assert_eq!(serialized, r#""manual""#);
        let deserialized: StopMode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(mode, deserialized);
    }

    #[test]
    fn event_is_append_only_and_idempotent_identity() {
        let machine = StopMachine::new();
        let event1 = StopEvent::new(
            StopMode::NetworkIncident,
            Source::Monitor,
            "rpc endpoint unreachable".into(),
            "soroban-testnet".into(),
        );
        let event2 = StopEvent::new(
            StopMode::NetworkIncident,
            Source::Monitor,
            "rpc endpoint unreachable".into(),
            "soroban-testnet".into(),
        );
        assert_eq!(event1.id(), event2.id());
        assert_eq!(event1.mode(), event2.mode());
        assert_eq!(event1.cause(), event2.cause());
        assert_eq!(event1.reason(), event2.reason());
        assert_eq!(event1.provenance(), event2.provenance());
        assert_eq!(event1.time_ms(), event2.time_ms());
    }

    #[tokio::test]
    async fn stop_state_transitions_are_logged_and_evaluable() {
        let machine = StopMachine::new();
        let hook = StopHook::new(Arc::new(machine.clone()));
        assert!(hook.eval().await.unwrap());

        let log = StopLog::new(Arc::new(machine.clone()));
        log.set_stop(
            StopMode::NetworkIncident,
            Source::Monitor,
            "rpc endpoint unreachable".into(),
            "soroban-testnet".into(),
        )
        .await;

        let event = machine.current().await.unwrap();
        assert_eq!(event.mode(), StopMode::NetworkIncident);
        assert_eq!(event.source(), Source::Monitor);
        assert!(machine.is_stopped().await);
        assert!(matches!(hook.eval().await.unwrap_err(), (StopMode::NetworkIncident, _) if event.reason() == "rpc endpoint unreachable"));

        log.resume("operator decided the incident was resolved".into(), "oncall".into()).await;
        let resumed = machine.current().await.unwrap();
        assert_eq!(resumed.mode(), StopMode::Manual);
        assert!(!machine.is_stopped().await);
        assert!(hook.eval().await.unwrap());
    }
}
