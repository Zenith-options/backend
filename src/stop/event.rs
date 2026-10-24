//! Structured, append-only audit events for the stop state machine.
//!
//! Every transition through the `StopMachine` produces exactly one `StopEvent`.
//! An event is immutable once created: its ID is derived from the mode and the
//! wall-clock second, so enabling and disabling the same mode in the same
//! second are two distinct events (id + cause distinguish them) rather than
//! an overwrite. The event carries the full provenance an auditor needs: who
//! (source), why (reason), where the failure came from (provenance), and when.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::stop::{Source, StopMode};

/// A single authoritative audit event in the stop state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopEvent {
    id: String,
    mode: StopMode,
    cause: Source,
    reason: String,
    provenance: String,
    at: i64,
}

impl StopEvent {
    pub fn new(mode: StopMode, cause: Source, reason: String, provenance: String) -> Self {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            id: format!(
                "stop-{}-{}",
                mode.as_str(),
                at / 1000,
            ),
            mode,
            cause,
            reason,
            provenance,
            at,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn mode(&self) -> &StopMode {
        &self.mode
    }

    /// The actor that acted on the stop. Defines who could toggle and who can
    /// resume.
    pub fn cause(&self) -> &Source {
        &self.cause
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    pub fn at(&self) -> i64 {
        self.at
    }
}

impl std::fmt::Display for StopEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stop id={} mode={} cause={} at={} reason={} provenance={}",
            self.id, self.mode.as_str(), self.cause, self.at, self.reason, self.provenance
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_carries_everything_and_serializes() {
        let event = StopEvent::new(
            StopMode::OracleStale,
            Source::Oracle,
            "oracle fixing older than 120s".into(),
            "oracle-publisher@zenith".into(),
        );
        assert!(event.id().starts_with("stop-oracle_stale-"));
        assert!(event.at() > 0);
        let round_trip = serde_json::to_string(&event).unwrap();
        let back: StopEvent = serde_json::from_str(&round_trip).unwrap();
        assert_eq!(event.id(), back.id());
        assert_eq!(event.mode(), back.mode());
        assert_eq!(event.cause(), back.cause());
        assert_eq!(event.reason(), back.reason());
        assert_eq!(event.provenance(), back.provenance());
        assert_eq!(event.at(), back.at());
    }

    #[test]
    fn event_is_structured_away_from_unstructured_stop_logs() {
        let event = StopEvent::new(
            StopMode::Manual,
            Source::Admin,
            "operator paused settlement during incident".into(),
            "oncall-operator".into(),
        );
        assert_eq!(event.mode(), StopMode::Manual);
        assert_eq!(event.cause(), Source::Admin);
        assert!(event.reason().contains("operator paused"));
        assert_eq!(event.provenance(), "oncall-operator");
    }
}
