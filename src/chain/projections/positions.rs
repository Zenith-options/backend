//! Position projection: fold OptionMinted and OptionExercised events into
//! projected_positions rows keyed by wallet:series_id.
use sqlx::{Sqlite, Transaction};

use crate::chain::bindings::events::DecodedEvent;

use super::{ProjectionError, StreamEvent, PROJECTOR_VERSION};

/// Public entry point used by the live apply path.
pub async fn apply_position(
    pool: &sqlx::SqlitePool,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    let mut tx = pool.begin().await?;
    apply_position_tx(&mut tx, ev).await?;
    tx.commit().await?;
    Ok(())
}

/// Fold a single event into the positions projection inside an existing tx.
/// Idempotent: re-applying the same (ledger, tx_hash, event_index) is a no-op.
pub async fn apply_position_tx(
    tx: &mut Transaction<'_, Sqlite>,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    match &ev.event {
        DecodedEvent::OptionMinted { series_id, strike, expiry, is_call, contracts, writer } => {
            let id = format!("{}:{}", writer, series_id);
            // Idempotency: skip if we've already processed this event coordinate.
            let already: Option<i64> = sqlx::query_scalar(
                "SELECT last_ledger FROM projected_positions
                 WHERE id=?1 AND last_ledger=?2 AND last_tx_hash=?3 AND last_event_index=?4",
            )
            .bind(&id).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
            .fetch_optional(&mut **tx).await?;
            if already.is_some() { return Ok(()); }

            sqlx::query(
                "INSERT INTO projected_positions
                     (id, wallet, series_id, contracts, is_call, strike, expiry, status,
                      projector_version, last_ledger, last_tx_hash, last_event_index)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'open',?8,?9,?10,?11)
                 ON CONFLICT(id) DO UPDATE SET
                     contracts       = projected_positions.contracts + excluded.contracts,
                     projector_version = excluded.projector_version,
                     last_ledger     = excluded.last_ledger,
                     last_tx_hash    = excluded.last_tx_hash,
                     last_event_index = excluded.last_event_index,
                     updated_at      = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            )
            .bind(&id).bind(writer).bind(series_id)
            .bind(contracts).bind(*is_call as i64)
            .bind(strike).bind(*expiry as i64)
            .bind(PROJECTOR_VERSION).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
            .execute(&mut **tx).await?;
        }

        DecodedEvent::OptionExercised { series_id, buyer, .. } => {
            let id = format!("{}:{}", buyer, series_id);
            let already: Option<i64> = sqlx::query_scalar(
                "SELECT last_ledger FROM projected_positions
                 WHERE id=?1 AND last_ledger=?2 AND last_tx_hash=?3 AND last_event_index=?4",
            )
            .bind(&id).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
            .fetch_optional(&mut **tx).await?;
            if already.is_some() { return Ok(()); }

            sqlx::query(
                "UPDATE projected_positions SET
                     status='exercised',
                     projector_version=?1,
                     last_ledger=?2, last_tx_hash=?3, last_event_index=?4,
                     updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id=?5 AND status='open'",
            )
            .bind(PROJECTOR_VERSION).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
            .bind(&id)
            .execute(&mut **tx).await?;
        }

        // Other event kinds don't affect positions.
        _ => {}
    }
    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::projections::StreamEvent;

    fn mint_event(series: &str, writer: &str, contracts: f64) -> StreamEvent {
        StreamEvent {
            ledger: 100, tx_hash: "txA".into(), event_index: 0,
            event: DecodedEvent::OptionMinted {
                series_id: series.into(), strike: 100.0, expiry: 9999, is_call: true,
                contracts, writer: writer.into(),
            },
        }
    }

    fn exercise_event(series: &str, buyer: &str) -> StreamEvent {
        StreamEvent {
            ledger: 101, tx_hash: "txB".into(), event_index: 0,
            event: DecodedEvent::OptionExercised {
                series_id: series.into(), buyer: buyer.into(), payout: 50.0,
            },
        }
    }

    async fn test_pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE projected_positions (
                id TEXT PRIMARY KEY, wallet TEXT NOT NULL, series_id TEXT NOT NULL,
                contracts REAL NOT NULL DEFAULT 0, is_call INTEGER NOT NULL DEFAULT 1,
                strike REAL NOT NULL DEFAULT 0, expiry INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'open',
                projector_version INTEGER NOT NULL DEFAULT 1,
                last_ledger INTEGER NOT NULL DEFAULT 0,
                last_tx_hash TEXT NOT NULL DEFAULT '',
                last_event_index INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT '')"
        ).execute(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn mint_inserts_open_position() {
        let pool = test_pool().await;
        apply_position(&pool, &mint_event("s1", "alice", 5.0)).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM projected_positions WHERE id='alice:s1'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(status, "open");
    }

    #[tokio::test]
    async fn mint_is_idempotent() {
        let pool = test_pool().await;
        let ev = mint_event("s1", "alice", 5.0);
        apply_position(&pool, &ev).await.unwrap();
        apply_position(&pool, &ev).await.unwrap(); // replay
        let contracts: f64 = sqlx::query_scalar("SELECT contracts FROM projected_positions WHERE id='alice:s1'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(contracts, 5.0, "replay must not double-count");
    }

    #[tokio::test]
    async fn exercise_closes_position() {
        let pool = test_pool().await;
        apply_position(&pool, &mint_event("s1", "alice", 5.0)).await.unwrap();
        apply_position(&pool, &exercise_event("s1", "alice")).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM projected_positions WHERE id='alice:s1'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(status, "exercised");
    }

    #[tokio::test]
    async fn multiple_mints_accumulate() {
        let pool = test_pool().await;
        apply_position(&pool, &mint_event("s1", "alice", 3.0)).await.unwrap();
        let ev2 = StreamEvent { ledger: 101, tx_hash: "txC".into(), event_index: 0,
            event: DecodedEvent::OptionMinted { series_id: "s1".into(), strike: 100.0,
                expiry: 9999, is_call: true, contracts: 2.0, writer: "alice".into() } };
        apply_position(&pool, &ev2).await.unwrap();
        let contracts: f64 = sqlx::query_scalar("SELECT contracts FROM projected_positions WHERE id='alice:s1'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(contracts, 5.0);
    }
}
