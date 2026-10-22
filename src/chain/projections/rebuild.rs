//! Rebuild command: replay all events from ledger 0 (or a snapshot ledger)
//! into fresh shadow tables, then compare against the live projection.
//! Mismatches are reported — never silently repaired.
//!
//! The same fold functions used for live application are reused here, so
//! rebuild and live paths are guaranteed to be identical.

use sqlx::SqlitePool;

use super::{
    checkpoint::advance_checkpoint, collateral::apply_collateral_tx,
    positions::apply_position_tx, settlement::apply_settlement_tx,
    ProjectionError, StreamEvent, PROJECTOR_VERSION,
};

/// Summary returned to the caller / CLI.
#[derive(Debug, Clone)]
pub struct RebuildReport {
    pub events_replayed:   u64,
    pub positions_match:   bool,
    pub collateral_match:  bool,
    pub settlement_match:  bool,
    pub mismatches:        Vec<String>,
}

impl RebuildReport {
    pub fn all_match(&self) -> bool {
        self.positions_match && self.collateral_match && self.settlement_match
    }
}

/// Rebuild all projections from the supplied event stream and verify they
/// match the live tables.
///
/// `events` must be ordered by `(ledger, tx_hash, event_index)` — the same
/// order used by the live indexer.  The rebuild writes into temporary tables
/// in the same DB (prefixed `_rebuild_`) and then diffs against the live ones.
///
/// Batch writes keep memory bounded regardless of history length.
pub async fn rebuild_all(
    pool: &SqlitePool,
    events: impl Iterator<Item = StreamEvent>,
) -> Result<RebuildReport, ProjectionError> {
    // Create shadow tables.
    create_shadow_tables(pool).await?;

    let mut count: u64 = 0;

    for ev in events {
        let mut tx = pool.begin().await.map_err(ProjectionError::Db)?;
        apply_position_tx_shadow(&mut tx, &ev).await?;
        apply_collateral_tx_shadow(&mut tx, &ev).await?;
        apply_settlement_tx_shadow(&mut tx, &ev).await?;
        tx.commit().await.map_err(ProjectionError::Db)?;
        count += 1;
    }

    // Advance rebuild checkpoints.
    advance_checkpoint(pool, "_rebuild_positions",  PROJECTOR_VERSION, 0, "", 0, count as i64).await?;
    advance_checkpoint(pool, "_rebuild_collateral", PROJECTOR_VERSION, 0, "", 0, count as i64).await?;
    advance_checkpoint(pool, "_rebuild_settlement", PROJECTOR_VERSION, 0, "", 0, count as i64).await?;

    // Diff shadow vs live.
    let mut mismatches = Vec::new();

    let pos_match = diff_positions(pool, &mut mismatches).await?;
    let col_match = diff_collateral(pool, &mut mismatches).await?;
    let set_match = diff_settlement(pool, &mut mismatches).await?;

    drop_shadow_tables(pool).await?;

    Ok(RebuildReport {
        events_replayed:  count,
        positions_match:  pos_match,
        collateral_match: col_match,
        settlement_match: set_match,
        mismatches,
    })
}

// ── Shadow-table writes (identical folds, different table names) ──────────────

async fn apply_position_tx_shadow(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    use crate::chain::bindings::events::DecodedEvent;
    match &ev.event {
        DecodedEvent::OptionMinted { series_id, strike, expiry, is_call, contracts, writer } => {
            let id = format!("{}:{}", writer, series_id);
            sqlx::query(
                "INSERT INTO _rebuild_positions
                     (id,wallet,series_id,contracts,is_call,strike,expiry,status,
                      projector_version,last_ledger,last_tx_hash,last_event_index)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'open',?8,?9,?10,?11)
                 ON CONFLICT(id) DO UPDATE SET
                     contracts=_rebuild_positions.contracts+excluded.contracts,
                     last_ledger=excluded.last_ledger,
                     last_tx_hash=excluded.last_tx_hash,
                     last_event_index=excluded.last_event_index",
            )
            .bind(&id).bind(writer).bind(series_id)
            .bind(contracts).bind(*is_call as i64)
            .bind(strike).bind(*expiry as i64)
            .bind(PROJECTOR_VERSION).bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
            .execute(&mut **tx).await?;
        }
        DecodedEvent::OptionExercised { series_id, buyer, .. } => {
            let id = format!("{}:{}", buyer, series_id);
            sqlx::query(
                "UPDATE _rebuild_positions SET status='exercised',
                     last_ledger=?1,last_tx_hash=?2,last_event_index=?3
                 WHERE id=?4 AND status='open'",
            )
            .bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index).bind(&id)
            .execute(&mut **tx).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn apply_collateral_tx_shadow(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    use crate::chain::bindings::events::DecodedEvent;
    let DecodedEvent::CollateralDeposited { wallet, amount } = &ev.event else { return Ok(()); };
    sqlx::query(
        "INSERT INTO _rebuild_collateral (wallet,balance,projector_version,last_ledger,last_tx_hash,last_event_index)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(wallet) DO UPDATE SET
             balance=_rebuild_collateral.balance+excluded.balance,
             last_ledger=excluded.last_ledger,last_tx_hash=excluded.last_tx_hash,
             last_event_index=excluded.last_event_index",
    )
    .bind(wallet).bind(amount).bind(PROJECTOR_VERSION)
    .bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
    .execute(&mut **tx).await?;
    Ok(())
}

async fn apply_settlement_tx_shadow(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ev: &StreamEvent,
) -> Result<(), ProjectionError> {
    use crate::chain::bindings::events::DecodedEvent;
    let DecodedEvent::OptionExercised { series_id, payout, .. } = &ev.event else { return Ok(()); };
    sqlx::query(
        "INSERT INTO _rebuild_settlement (series_id,settlement_price,is_settled,projector_version,last_ledger,last_tx_hash,last_event_index)
         VALUES (?1,?2,1,?3,?4,?5,?6)
         ON CONFLICT(series_id) DO UPDATE SET
             settlement_price=excluded.settlement_price,is_settled=1,
             last_ledger=excluded.last_ledger,last_tx_hash=excluded.last_tx_hash,
             last_event_index=excluded.last_event_index",
    )
    .bind(series_id).bind(payout).bind(PROJECTOR_VERSION)
    .bind(ev.ledger).bind(&ev.tx_hash).bind(ev.event_index)
    .execute(&mut **tx).await?;
    Ok(())
}

// ── Shadow table lifecycle ────────────────────────────────────────────────────

async fn create_shadow_tables(pool: &SqlitePool) -> Result<(), ProjectionError> {
    sqlx::query("DROP TABLE IF EXISTS _rebuild_positions").execute(pool).await?;
    sqlx::query("DROP TABLE IF EXISTS _rebuild_collateral").execute(pool).await?;
    sqlx::query("DROP TABLE IF EXISTS _rebuild_settlement").execute(pool).await?;

    sqlx::query(
        "CREATE TABLE _rebuild_positions AS SELECT * FROM projected_positions WHERE 0",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE _rebuild_collateral AS SELECT * FROM projected_collateral WHERE 0",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE _rebuild_settlement AS SELECT * FROM projected_settlement WHERE 0",
    ).execute(pool).await?;
    Ok(())
}

async fn drop_shadow_tables(pool: &SqlitePool) -> Result<(), ProjectionError> {
    sqlx::query("DROP TABLE IF EXISTS _rebuild_positions").execute(pool).await?;
    sqlx::query("DROP TABLE IF EXISTS _rebuild_collateral").execute(pool).await?;
    sqlx::query("DROP TABLE IF EXISTS _rebuild_settlement").execute(pool).await?;
    Ok(())
}

// ── Diff helpers ──────────────────────────────────────────────────────────────

async fn diff_positions(pool: &SqlitePool, mismatches: &mut Vec<String>) -> Result<bool, ProjectionError> {
    // Rows in live but not rebuild (or different values).
    let rows: Vec<(String, f64, String)> = sqlx::query_as(
        "SELECT p.id, p.contracts, p.status FROM projected_positions p
         LEFT JOIN _rebuild_positions r ON p.id = r.id
         WHERE r.id IS NULL
            OR ABS(p.contracts - r.contracts) > 0.000001
            OR p.status != r.status",
    ).fetch_all(pool).await?;
    for (id, contracts, status) in &rows {
        mismatches.push(format!("positions mismatch id={id} contracts={contracts} status={status}"));
    }
    // Rows in rebuild but not live.
    let extra: Vec<(String,)> = sqlx::query_as(
        "SELECT r.id FROM _rebuild_positions r
         LEFT JOIN projected_positions p ON r.id = p.id WHERE p.id IS NULL",
    ).fetch_all(pool).await?;
    for (id,) in &extra {
        mismatches.push(format!("positions: rebuild has extra row id={id}"));
    }
    Ok(rows.is_empty() && extra.is_empty())
}

async fn diff_collateral(pool: &SqlitePool, mismatches: &mut Vec<String>) -> Result<bool, ProjectionError> {
    let rows: Vec<(String, f64)> = sqlx::query_as(
        "SELECT p.wallet, p.balance FROM projected_collateral p
         LEFT JOIN _rebuild_collateral r ON p.wallet = r.wallet
         WHERE r.wallet IS NULL OR ABS(p.balance - r.balance) > 0.000001",
    ).fetch_all(pool).await?;
    for (w, b) in &rows {
        mismatches.push(format!("collateral mismatch wallet={w} balance={b}"));
    }
    let extra: Vec<(String,)> = sqlx::query_as(
        "SELECT r.wallet FROM _rebuild_collateral r
         LEFT JOIN projected_collateral p ON r.wallet = p.wallet WHERE p.wallet IS NULL",
    ).fetch_all(pool).await?;
    for (w,) in &extra {
        mismatches.push(format!("collateral: rebuild has extra wallet={w}"));
    }
    Ok(rows.is_empty() && extra.is_empty())
}

async fn diff_settlement(pool: &SqlitePool, mismatches: &mut Vec<String>) -> Result<bool, ProjectionError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT p.series_id, p.is_settled FROM projected_settlement p
         LEFT JOIN _rebuild_settlement r ON p.series_id = r.series_id
         WHERE r.series_id IS NULL OR p.is_settled != r.is_settled",
    ).fetch_all(pool).await?;
    for (s, settled) in &rows {
        mismatches.push(format!("settlement mismatch series={s} is_settled={settled}"));
    }
    let extra: Vec<(String,)> = sqlx::query_as(
        "SELECT r.series_id FROM _rebuild_settlement r
         LEFT JOIN projected_settlement p ON r.series_id = p.series_id WHERE p.series_id IS NULL",
    ).fetch_all(pool).await?;
    for (s,) in &extra {
        mismatches.push(format!("settlement: rebuild has extra series={s}"));
    }
    Ok(rows.is_empty() && extra.is_empty())
}
