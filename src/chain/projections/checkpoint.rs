//! Per-projection checkpoints — advance after a successful commit, read on resume.
use sqlx::SqlitePool;
use super::ProjectionError;

#[derive(Debug, Clone)]
pub struct ProjectionCheckpoint {
    pub projection:         String,
    pub projector_version:  i64,
    pub ledger:             i64,
    pub tx_hash:            String,
    pub event_index:        i64,
    pub events_applied:     i64,
}

/// Read the latest checkpoint for a projection at a given version.
/// Returns None if no events have been applied yet.
pub async fn read_checkpoint(
    pool: &SqlitePool,
    projection: &str,
    version: i64,
) -> Result<Option<ProjectionCheckpoint>, ProjectionError> {
    let row = sqlx::query_as!(
        ProjectionCheckpoint,
        "SELECT projection, projector_version, ledger, tx_hash, event_index, events_applied
         FROM projection_checkpoints
         WHERE projection = ?1 AND projector_version = ?2",
        projection, version,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Advance (upsert) the checkpoint after a successful projection commit.
pub async fn advance_checkpoint(
    pool: &SqlitePool,
    projection: &str,
    version: i64,
    ledger: i64,
    tx_hash: &str,
    event_index: i64,
    events_applied_delta: i64,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO projection_checkpoints
             (projection, projector_version, ledger, tx_hash, event_index, events_applied)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(projection, projector_version) DO UPDATE SET
             ledger          = excluded.ledger,
             tx_hash         = excluded.tx_hash,
             event_index     = excluded.event_index,
             events_applied  = projection_checkpoints.events_applied + excluded.events_applied,
             updated_at      = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(projection).bind(version).bind(ledger).bind(tx_hash)
    .bind(event_index).bind(events_applied_delta)
    .execute(pool)
    .await?;
    Ok(())
}
