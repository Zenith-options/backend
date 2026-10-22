//! Read-model projection layer.
//!
//! Events are pure inputs; projections are deterministic folds over an ordered
//! event stream keyed by `(ledger, tx_hash, event_index)`. The same fold
//! functions are used for live application and for rebuilds — so a rebuild
//! from ledger 0 must produce byte-identical rows.
//!
//! Modules:
//!   positions   — open/closed position legs per wallet × series
//!   collateral  — collateral balance per wallet
//!   settlement  — settlement status per series
//!   checkpoint  — advance/read per-projection cursors
//!   rebuild     — full rebuild command with equivalence check

pub mod checkpoint;
pub mod collateral;
pub mod positions;
pub mod rebuild;
pub mod settlement;

pub use checkpoint::{advance_checkpoint, read_checkpoint, ProjectionCheckpoint};
pub use collateral::apply_collateral;
pub use positions::apply_position;
pub use rebuild::{rebuild_all, RebuildReport};
pub use settlement::apply_settlement;

use crate::chain::bindings::events::DecodedEvent;
use sqlx::SqlitePool;
use uuid::Uuid;

/// Current projector version. Bump this when any fold logic changes to
/// trigger a required rebuild.
pub const PROJECTOR_VERSION: i64 = 1;

/// An event from the stream together with its idempotency coordinates.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub ledger:      i64,
    pub tx_hash:     String,
    pub event_index: i64,
    pub event:       DecodedEvent,
}

/// Apply one event to all three projections inside a single DB transaction.
/// Idempotent: duplicate `(ledger, tx_hash, event_index)` tuples are no-ops.
pub async fn apply_event(pool: &SqlitePool, ev: &StreamEvent) -> Result<(), ProjectionError> {
    let mut tx = pool.begin().await.map_err(ProjectionError::Db)?;

    // Each apply_ function does an idempotency guard internally.
    if let Err(e) = positions::apply_position_tx(&mut tx, ev).await {
        quarantine(&mut tx, ev, &e.to_string()).await?;
    }
    if let Err(e) = collateral::apply_collateral_tx(&mut tx, ev).await {
        quarantine(&mut tx, ev, &e.to_string()).await?;
    }
    if let Err(e) = settlement::apply_settlement_tx(&mut tx, ev).await {
        quarantine(&mut tx, ev, &e.to_string()).await?;
    }

    tx.commit().await.map_err(ProjectionError::Db)?;
    Ok(())
}

/// Quarantine an unapplicable event without crashing the indexer.
async fn quarantine(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ev: &StreamEvent,
    reason: &str,
) -> Result<(), ProjectionError> {
    let id = Uuid::new_v4().to_string();
    let json = serde_json::to_string(&ev.event).unwrap_or_default();
    sqlx::query(
        "INSERT OR IGNORE INTO projection_quarantine
             (id, ledger, tx_hash, event_index, event_json, reason)
         VALUES (?1,?2,?3,?4,?5,?6)",
    )
    .bind(&id)
    .bind(ev.ledger)
    .bind(&ev.tx_hash)
    .bind(ev.event_index)
    .bind(&json)
    .bind(reason)
    .execute(&mut **tx)
    .await
    .map_err(ProjectionError::Db)?;
    Ok(())
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ProjectionError {
    Db(sqlx::Error),
    UnknownSeries(String),
    UnknownWallet(String),
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e)              => write!(f, "db: {e}"),
            Self::UnknownSeries(s)   => write!(f, "unknown series: {s}"),
            Self::UnknownWallet(w)   => write!(f, "unknown wallet: {w}"),
        }
    }
}

impl From<sqlx::Error> for ProjectionError {
    fn from(e: sqlx::Error) -> Self { Self::Db(e) }
}
