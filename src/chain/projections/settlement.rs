//! Settlement projection: no DecodedEvent variant for settlement yet, so we
//! model it via OptionExercised as a proxy and expose a hook for future
//! Settled events. The table is already created in the migration.
use sqlx::{Sqlite, Transaction};

use crate::chain::bindings::events::DecodedEvent;

use super::{ProjectionError, StreamEvent, PROJECTOR_VERSION};

pub async fn apply_settlement(
    pool: &sqlx::SqlitePool,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    let mut tx = pool.begin().await?;
    apply_settlement_tx(&mut tx, ev).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn apply_settlement_tx(
    tx: &mut Transaction<'_, Sqlite>,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    // Currently OptionExercised is the closest event to settlement we have.
    // When a dedicated Settled event lands it will be wired here.
    let (series_id, payout) = match &ev.event {
        DecodedEvent::OptionExercised { series_id, payout, .. } => (series_id.clone(), *payout),
        _ => return Ok(()),
    };

    // Idempotency check.
    let already: Option<i64> = sqlx::query_scalar(
        "SELECT last_ledger FROM projected_settlement
         WHERE series_id=?1 AND last_ledger=?2 AND last_tx_hash=?3 AND last_event_index=?4",
    )
    .bind(&series_id).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
    .fetch_optional(&mut **tx).await?;
    if already.is_some() { return Ok(()); }

    sqlx::query(
        "INSERT INTO projected_settlement
             (series_id, settlement_price, is_settled,
              projector_version, last_ledger, last_tx_hash, last_event_index)
         VALUES (?1,?2,1,?3,?4,?5,?6)
         ON CONFLICT(series_id) DO UPDATE SET
             settlement_price  = excluded.settlement_price,
             is_settled        = 1,
             projector_version = excluded.projector_version,
             last_ledger       = excluded.last_ledger,
             last_tx_hash      = excluded.last_tx_hash,
             last_event_index  = excluded.last_event_index,
             updated_at        = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(&series_id).bind(payout)
    .bind(PROJECTOR_VERSION).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
    .execute(&mut **tx).await?;

    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE projected_settlement (
                series_id TEXT PRIMARY KEY, settlement_price REAL,
                is_settled INTEGER NOT NULL DEFAULT 0,
                projector_version INTEGER NOT NULL DEFAULT 1,
                last_ledger INTEGER NOT NULL DEFAULT 0,
                last_tx_hash TEXT NOT NULL DEFAULT '',
                last_event_index INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT '')"
        ).execute(&pool).await.unwrap();
        pool
    }

    fn exercise(series: &str, buyer: &str, payout: f64, ledger: i64, idx: i64) -> StreamEvent {
        StreamEvent {
            ledger, tx_hash: format!("tx{ledger}"), event_index: idx,
            event: DecodedEvent::OptionExercised { series_id: series.into(), buyer: buyer.into(), payout },
        }
    }

    #[tokio::test]
    async fn exercise_marks_series_settled() {
        let pool = test_pool().await;
        apply_settlement(&pool, &exercise("s1", "alice", 100.0, 1, 0)).await.unwrap();
        let settled: i64 = sqlx::query_scalar("SELECT is_settled FROM projected_settlement WHERE series_id='s1'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(settled, 1);
    }

    #[tokio::test]
    async fn settlement_is_idempotent() {
        let pool = test_pool().await;
        let ev = exercise("s1", "alice", 100.0, 1, 0);
        apply_settlement(&pool, &ev).await.unwrap();
        apply_settlement(&pool, &ev).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projected_settlement")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }
}
