//! Collateral projection: fold CollateralDeposited into projected_collateral.
use sqlx::{Sqlite, Transaction};

use crate::chain::bindings::events::DecodedEvent;

use super::{ProjectionError, StreamEvent, PROJECTOR_VERSION};

pub async fn apply_collateral(
    pool: &sqlx::SqlitePool,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    let mut tx = pool.begin().await?;
    apply_collateral_tx(&mut tx, ev).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn apply_collateral_tx(
    tx: &mut Transaction<'_, Sqlite>,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    let DecodedEvent::CollateralDeposited { wallet, amount } = &ev.event else {
        return Ok(());
    };

    // Idempotency check.
    let already: Option<i64> = sqlx::query_scalar(
        "SELECT last_ledger FROM projected_collateral
         WHERE wallet=?1 AND last_ledger=?2 AND last_tx_hash=?3 AND last_event_index=?4",
    )
    .bind(wallet).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
    .fetch_optional(&mut **tx).await?;
    if already.is_some() { return Ok(()); }

    sqlx::query(
        "INSERT INTO projected_collateral
             (wallet, balance, projector_version, last_ledger, last_tx_hash, last_event_index)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(wallet) DO UPDATE SET
             balance           = projected_collateral.balance + excluded.balance,
             projector_version = excluded.projector_version,
             last_ledger       = excluded.last_ledger,
             last_tx_hash      = excluded.last_tx_hash,
             last_event_index  = excluded.last_event_index,
             updated_at        = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(wallet).bind(amount)
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
            "CREATE TABLE projected_collateral (
                wallet TEXT PRIMARY KEY, balance REAL NOT NULL DEFAULT 0,
                projector_version INTEGER NOT NULL DEFAULT 1,
                last_ledger INTEGER NOT NULL DEFAULT 0,
                last_tx_hash TEXT NOT NULL DEFAULT '',
                last_event_index INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT '')"
        ).execute(&pool).await.unwrap();
        pool
    }

    fn deposit(wallet: &str, amount: f64, ledger: i64, idx: i64) -> StreamEvent {
        StreamEvent {
            ledger, tx_hash: format!("tx{ledger}"), event_index: idx,
            event: DecodedEvent::CollateralDeposited { wallet: wallet.into(), amount },
        }
    }

    #[tokio::test]
    async fn deposit_creates_row() {
        let pool = test_pool().await;
        apply_collateral(&pool, &deposit("alice", 500.0, 1, 0)).await.unwrap();
        let bal: f64 = sqlx::query_scalar("SELECT balance FROM projected_collateral WHERE wallet='alice'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(bal, 500.0);
    }

    #[tokio::test]
    async fn deposit_is_idempotent() {
        let pool = test_pool().await;
        let ev = deposit("alice", 500.0, 1, 0);
        apply_collateral(&pool, &ev).await.unwrap();
        apply_collateral(&pool, &ev).await.unwrap();
        let bal: f64 = sqlx::query_scalar("SELECT balance FROM projected_collateral WHERE wallet='alice'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(bal, 500.0, "replay must not double-count");
    }

    #[tokio::test]
    async fn two_deposits_accumulate() {
        let pool = test_pool().await;
        apply_collateral(&pool, &deposit("alice", 300.0, 1, 0)).await.unwrap();
        apply_collateral(&pool, &deposit("alice", 200.0, 2, 0)).await.unwrap();
        let bal: f64 = sqlx::query_scalar("SELECT balance FROM projected_collateral WHERE wallet='alice'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(bal, 500.0);
    }
}
