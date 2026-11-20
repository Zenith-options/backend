//! Margin-call lifecycle state machine.
//!
//! Lifecycle:
//!  1. **Detection** – [`detect_deficit`] is pure: given an
//!     [`AccountMarginSnapshot`] it returns a [`MarginDeficit`] with no I/O.
//!  2. **Opening**   – [`open_call`] persists the record + outbox event inside
//!     the caller's transaction. Per-account cooldown prevents call spam.
//!  3. **Re-check**  – [`recheck`] evaluates cure on every relevant event
//!     (deposit, fill, mark-price move) and on the periodic sweep.
//!  4. **Expiry**    – [`expire_overdue`] is called by the keeper sweep; it
//!     escalates each expired call to the liquidation engine exactly once,
//!     guarded by a `WHERE status = 'open'` CAS so cure and expire cannot
//!     both win.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::outbox::{self, DomainEvent, EVENT_VERSION};

// ─── Policy ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarginCallPolicy {
    /// Wall-clock cure window in seconds.
    pub grace_period_secs: u64,
    /// Ledger-height cure window (on-chain agreement).
    pub grace_period_ledgers: u64,
    /// Fraction of required_top_up that must be supplied to count as cured
    /// (1.0 = full cure required).
    pub min_cure_fraction: f64,
    /// Deficit (10^7 units) below which partial-deleverage is used instead of
    /// full liquidation on expiry.
    pub partial_deleverage_threshold: i64,
    /// Minimum seconds between two calls on the same account (anti-spam).
    pub per_account_cooldown_secs: u64,
    /// Fraction of hysteresis: account must recover this fraction above the
    /// trigger before a new call can be opened to prevent oscillation.
    pub hysteresis_fraction: f64,
}

impl Default for MarginCallPolicy {
    fn default() -> Self {
        Self {
            grace_period_secs: 3600,
            grace_period_ledgers: 720,
            min_cure_fraction: 1.0,
            partial_deleverage_threshold: 10_000_000,
            per_account_cooldown_secs: 300,
            hysteresis_fraction: 0.02,
        }
    }
}

// ─── Domain types ─────────────────────────────────────────────────────────────

/// A margin deficit produced by [`detect_deficit`].
#[derive(Debug, Clone, PartialEq)]
pub struct MarginDeficit {
    pub wallet_address: String,
    /// Shortfall in 10^7 fixed-point units (always > 0).
    pub deficit: i64,
    /// Amount needed to restore initial margin (10^7 units).
    pub required_top_up: i64,
    /// Ledger sequence at detection time.
    pub ledger: u64,
}

/// A persisted margin-call record.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MarginCall {
    pub id: String,
    pub wallet_address: String,
    pub deficit: i64,
    pub required_top_up: i64,
    pub opened_at: String,
    pub cure_deadline: String,
    pub opened_at_ledger: i64,
    pub deadline_ledger: i64,
    pub status: String,
    pub resolved_at: Option<String>,
    pub resolved_at_ledger: Option<i64>,
    pub cured_amount: Option<i64>,
    pub note: Option<String>,
    pub previous_call_id: Option<String>,
    pub created_at: String,
}

/// Errors returned by this module.
#[derive(Debug, thiserror::Error)]
pub enum MarginCallError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("cooldown active: last call {secs}s ago, cooldown is {cooldown_secs}s")]
    CooldownActive { secs: i64, cooldown_secs: u64 },
}

// ─── Detection (pure, no I/O) ─────────────────────────────────────────────────

/// Input snapshot for the pure deficit detector.
#[derive(Debug, Clone)]
pub struct AccountMarginSnapshot {
    pub wallet_address: String,
    /// Current equity in 10^7 fixed-point units.
    pub equity: i64,
    pub initial_margin: i64,
    pub maintenance_margin: i64,
    pub ledger: u64,
}

/// Returns a [`MarginDeficit`] when equity < maintenance_margin, else `None`.
/// Pure function: no I/O, safe to call from fuzz/unit harnesses.
pub fn detect_deficit(snap: &AccountMarginSnapshot) -> Option<MarginDeficit> {
    if snap.equity >= snap.maintenance_margin {
        return None;
    }
    Some(MarginDeficit {
        wallet_address: snap.wallet_address.clone(),
        deficit: snap.maintenance_margin.saturating_sub(snap.equity).max(0),
        required_top_up: snap.initial_margin.saturating_sub(snap.equity).max(0),
        ledger: snap.ledger,
    })
}

// ─── Timestamp helpers ────────────────────────────────────────────────────────

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let dt = time::OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| secs.to_string())
}

fn iso_plus_secs(secs: u64) -> String {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let dt = time::OffsetDateTime::from_unix_timestamp((base + secs) as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| (base + secs).to_string())
}

fn parse_unix(iso: &str) -> i64 {
    time::OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339)
        .map(|dt| dt.unix_timestamp())
        .unwrap_or(0)
}

// ─── Open ─────────────────────────────────────────────────────────────────────

/// Open a new margin call.  Checks cooldown, inserts the record, and writes
/// the `margin_call_opened` outbox event — all inside `tx`.
pub async fn open_call(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    deficit: &MarginDeficit,
    policy: &MarginCallPolicy,
) -> Result<MarginCall, MarginCallError> {
    // Per-account cooldown guard.
    let last: Option<(String, String)> = sqlx::query_as(
        "SELECT id, opened_at FROM margin_calls
          WHERE wallet_address = ? ORDER BY opened_at DESC LIMIT 1",
    )
    .bind(&deficit.wallet_address)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some((_, prev_opened_at)) = &last {
        let elapsed = now_unix() - parse_unix(prev_opened_at);
        if elapsed < policy.per_account_cooldown_secs as i64 {
            return Err(MarginCallError::CooldownActive {
                secs: elapsed,
                cooldown_secs: policy.per_account_cooldown_secs,
            });
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let opened_at = now_iso();
    let cure_deadline = iso_plus_secs(policy.grace_period_secs);
    let deadline_ledger = deficit.ledger + policy.grace_period_ledgers;
    let previous_call_id = last.map(|(id, _)| id);

    sqlx::query(
        "INSERT INTO margin_calls
            (id, wallet_address, deficit, required_top_up,
             opened_at, cure_deadline, opened_at_ledger, deadline_ledger,
             status, previous_call_id)
         VALUES (?,?,?,?,?,?,?,?,'open',?)",
    )
    .bind(&id)
    .bind(&deficit.wallet_address)
    .bind(deficit.deficit)
    .bind(deficit.required_top_up)
    .bind(&opened_at)
    .bind(&cure_deadline)
    .bind(deficit.ledger as i64)
    .bind(deadline_ledger as i64)
    .bind(&previous_call_id)
    .execute(&mut **tx)
    .await?;

    let event = DomainEvent::MarginCallOpened(crate::outbox::MarginCallOpenedPayload {
        version: EVENT_VERSION,
        call_id: id.clone(),
        wallet_address: deficit.wallet_address.clone(),
        deficit: deficit.deficit,
        required_top_up: deficit.required_top_up,
        cure_deadline: cure_deadline.clone(),
        deadline_ledger: deadline_ledger as i64,
        opened_at: opened_at.clone(),
    });
    outbox::emit(tx, &event).await?;

    Ok(MarginCall {
        id,
        wallet_address: deficit.wallet_address.clone(),
        deficit: deficit.deficit,
        required_top_up: deficit.required_top_up,
        opened_at,
        cure_deadline,
        opened_at_ledger: deficit.ledger as i64,
        deadline_ledger: deadline_ledger as i64,
        status: "open".into(),
        resolved_at: None,
        resolved_at_ledger: None,
        cured_amount: None,
        note: None,
        previous_call_id,
        created_at: now_iso(),
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ─── Re-check ────────────────────────────────────────────────────────────────

/// Re-evaluate the open call for `wallet_address` against a fresh snapshot.
/// Transitions to `cured`/`partially_cured` when equity ≥ maintenance; leaves
/// it `open` otherwise.  Returns `None` when no open call exists.
///
/// Uses `WHERE status = 'open'` CAS: if `expire_overdue` wins the race first,
/// `rows_affected == 0` and we return `Ok(None)`.
pub async fn recheck(
    db: &SqlitePool,
    wallet_address: &str,
    snap: &AccountMarginSnapshot,
    policy: &MarginCallPolicy,
    current_ledger: u64,
) -> Result<Option<MarginCall>, MarginCallError> {
    let call: Option<MarginCall> = sqlx::query_as(
        "SELECT * FROM margin_calls
          WHERE wallet_address = ? AND status = 'open'
          ORDER BY opened_at DESC LIMIT 1",
    )
    .bind(wallet_address)
    .fetch_optional(db)
    .await?;

    let Some(call) = call else { return Ok(None) };

    if snap.equity < snap.maintenance_margin {
        return Ok(Some(call)); // still under water
    }

    let resolved_at = now_iso();
    let surplus = snap.equity - snap.maintenance_margin;
    let fraction = if call.required_top_up > 0 {
        surplus as f64 / call.required_top_up as f64
    } else {
        1.0
    };
    let new_status = if fraction >= policy.min_cure_fraction {
        "cured"
    } else {
        "partially_cured"
    };

    let mut tx = db.begin().await?;
    let rows = sqlx::query(
        "UPDATE margin_calls
            SET status=?, resolved_at=?, resolved_at_ledger=?,
                cured_amount=?, note='cured by re-check'
          WHERE id=? AND status='open'",
    )
    .bind(new_status)
    .bind(&resolved_at)
    .bind(current_ledger as i64)
    .bind(surplus)
    .bind(&call.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if rows == 0 {
        tx.rollback().await?;
        return Ok(None);
    }

    let event = DomainEvent::MarginCallResolved(crate::outbox::MarginCallResolvedPayload {
        version: EVENT_VERSION,
        call_id: call.id.clone(),
        wallet_address: wallet_address.to_string(),
        outcome: new_status.to_string(),
        resolved_at: resolved_at.clone(),
        resolved_at_ledger: current_ledger as i64,
        cured_amount: Some(surplus),
    });
    outbox::emit(&mut tx, &event).await?;
    tx.commit().await?;

    Ok(Some(MarginCall {
        status: new_status.to_string(),
        resolved_at: Some(resolved_at),
        resolved_at_ledger: Some(current_ledger as i64),
        cured_amount: Some(surplus),
        note: Some("cured by re-check".into()),
        ..call
    }))
}

// ─── Expiry / escalation ──────────────────────────────────────────────────────

/// Callback the sweep calls when a call must be handed to the liquidation
/// engine.  Runs inside the same transaction as the status update.
pub trait LiquidationTrigger: Send + Sync {
    fn trigger<'a>(
        &'a self,
        call: &'a MarginCall,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'a>>;
}

/// Scan all open calls past both their wall-clock and ledger deadlines and
/// escalate each one exactly once.  Returns the ids of escalated calls.
pub async fn expire_overdue(
    db: &SqlitePool,
    policy: &MarginCallPolicy,
    current_ledger: u64,
    trigger: &dyn LiquidationTrigger,
) -> Result<Vec<String>, MarginCallError> {
    let now = now_iso();
    let expired: Vec<MarginCall> = sqlx::query_as(
        "SELECT * FROM margin_calls
          WHERE status='open' AND cure_deadline <= ?
          ORDER BY cure_deadline ASC",
    )
    .bind(&now)
    .fetch_all(db)
    .await?;

    let mut escalated = Vec::new();

    for call in expired {
        if call.deadline_ledger as u64 > current_ledger {
            continue;
        }

        let resolved_at = now_iso();
        let new_status = if call.deficit < policy.partial_deleverage_threshold {
            "expired"
        } else {
            "escalated"
        };

        let mut tx = db.begin().await?;
        let rows = sqlx::query(
            "UPDATE margin_calls
                SET status=?, resolved_at=?, resolved_at_ledger=?,
                    note='deadline passed; handed to liquidation engine'
              WHERE id=? AND status='open'",
        )
        .bind(new_status)
        .bind(&resolved_at)
        .bind(current_ledger as i64)
        .bind(&call.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if rows == 0 {
            tx.rollback().await?;
            continue; // cure won the race
        }

        let event = DomainEvent::MarginCallEscalated(crate::outbox::MarginCallEscalatedPayload {
            version: EVENT_VERSION,
            call_id: call.id.clone(),
            wallet_address: call.wallet_address.clone(),
            outcome: new_status.to_string(),
            resolved_at: resolved_at.clone(),
            resolved_at_ledger: current_ledger as i64,
            deficit: call.deficit,
        });
        if let Err(e) = outbox::emit(&mut tx, &event).await {
            tx.rollback().await?;
            tracing::warn!(call_id = %call.id, error = %e, "escalation event failed; rolling back");
            continue;
        }
        tx.commit().await?;

        // Fire the external liquidation trigger (best-effort; already committed).
        if let Err(e) = trigger.trigger(&call).await {
            tracing::warn!(call_id = %call.id, error = %e, "liquidation trigger error");
        }

        escalated.push(call.id.clone());
        tracing::info!(call_id = %call.id, wallet = %call.wallet_address, outcome = new_status, "margin call escalated");
    }

    Ok(escalated)
}

// ─── Query helpers ────────────────────────────────────────────────────────────

pub async fn get_open_call(
    db: &SqlitePool,
    wallet_address: &str,
) -> Result<Option<MarginCall>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM margin_calls
          WHERE wallet_address=? AND status='open'
          ORDER BY opened_at DESC LIMIT 1",
    )
    .bind(wallet_address)
    .fetch_optional(db)
    .await
}

pub async fn list_calls(
    db: &SqlitePool,
    wallet_address: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<MarginCall>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM margin_calls
          WHERE wallet_address=?
          ORDER BY opened_at DESC LIMIT ? OFFSET ?",
    )
    .bind(wallet_address)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
}

// ─── Default no-op trigger ────────────────────────────────────────────────────

/// Used by the default sweep until the production liquidation engine hooks in.
pub struct NoopLiquidationTrigger;

impl LiquidationTrigger for NoopLiquidationTrigger {
    fn trigger<'a>(
        &'a self,
        _call: &'a MarginCall,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'a>>
    {
        Box::pin(async {
            tracing::info!("NoopLiquidationTrigger: no liquidation engine wired yet");
            Ok(())
        })
    }
}

// ─── Periodic sweep ───────────────────────────────────────────────────────────

pub async fn sweep_loop(
    db: SqlitePool,
    policy: MarginCallPolicy,
    cadence: std::time::Duration,
    trigger: std::sync::Arc<dyn LiquidationTrigger>,
) {
    let mut interval = tokio::time::interval(cadence);
    loop {
        interval.tick().await;
        let current_ledger: u64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) FROM ledger_checkpoints",
        )
        .fetch_one(&db)
        .await
        .unwrap_or(u64::MAX);

        match expire_overdue(&db, &policy, current_ledger, trigger.as_ref()).await {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(count = ids.len(), "margin call sweep: escalated");
            }
            Err(e) => tracing::warn!(error = %e, "margin call sweep error"),
            _ => {}
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure detection ────────────────────────────────────────────────────────

    #[test]
    fn detect_healthy_returns_none() {
        let snap = AccountMarginSnapshot {
            wallet_address: "ALICE".into(),
            equity: 1_000_000_000,
            initial_margin: 800_000_000,
            maintenance_margin: 600_000_000,
            ledger: 100,
        };
        assert!(detect_deficit(&snap).is_none());
    }

    #[test]
    fn detect_at_maintenance_returns_none() {
        let snap = AccountMarginSnapshot {
            wallet_address: "ALICE".into(),
            equity: 600_000_000,
            initial_margin: 800_000_000,
            maintenance_margin: 600_000_000,
            ledger: 100,
        };
        assert!(detect_deficit(&snap).is_none());
    }

    #[test]
    fn detect_breach_returns_correct_figures() {
        let snap = AccountMarginSnapshot {
            wallet_address: "ALICE".into(),
            equity: 400_000_000,
            initial_margin: 800_000_000,
            maintenance_margin: 600_000_000,
            ledger: 42,
        };
        let d = detect_deficit(&snap).unwrap();
        assert_eq!(d.deficit, 200_000_000);
        assert_eq!(d.required_top_up, 400_000_000);
        assert_eq!(d.ledger, 42);
    }

    // ── State machine (SQLite) ────────────────────────────────────────────────

    async fn test_pool() -> (SqlitePool, std::path::PathBuf) {
        let path = std::env::temp_dir()
            .join(format!("zenith-mc-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", path.display())).await;
        (pool, path)
    }

    fn fast_policy() -> MarginCallPolicy {
        MarginCallPolicy {
            per_account_cooldown_secs: 0,
            ..MarginCallPolicy::default()
        }
    }

    #[tokio::test]
    async fn open_call_persists_record() {
        let (pool, path) = test_pool().await;
        let deficit = MarginDeficit {
            wallet_address: "ALICE".into(),
            deficit: 200_000_000,
            required_top_up: 400_000_000,
            ledger: 100,
        };
        let mut tx = pool.begin().await.unwrap();
        let call = open_call(&mut tx, &deficit, &fast_policy()).await.unwrap();
        tx.commit().await.unwrap();

        let loaded: MarginCall = sqlx::query_as(
            "SELECT * FROM margin_calls WHERE id=?",
        )
        .bind(&call.id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(loaded.status, "open");
        assert_eq!(loaded.deficit, 200_000_000);
        assert_eq!(loaded.deadline_ledger, 820);

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn recheck_cures_recovered_account() {
        let (pool, path) = test_pool().await;
        let policy = fast_policy();
        let deficit = MarginDeficit {
            wallet_address: "ALICE".into(),
            deficit: 200_000_000,
            required_top_up: 400_000_000,
            ledger: 100,
        };
        let mut tx = pool.begin().await.unwrap();
        open_call(&mut tx, &deficit, &policy).await.unwrap();
        tx.commit().await.unwrap();

        let snap = AccountMarginSnapshot {
            wallet_address: "ALICE".into(),
            equity: 1_000_000_000,
            initial_margin: 800_000_000,
            maintenance_margin: 600_000_000,
            ledger: 110,
        };
        let updated = recheck(&pool, "ALICE", &snap, &policy, 110)
            .await.unwrap().unwrap();
        assert_eq!(updated.status, "cured");
        assert_eq!(updated.resolved_at_ledger, Some(110));

        // Second recheck on a closed call → None.
        assert!(recheck(&pool, "ALICE", &snap, &policy, 115).await.unwrap().is_none());

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn expire_escalates_past_deadline() {
        let (pool, path) = test_pool().await;
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO margin_calls
                (id,wallet_address,deficit,required_top_up,
                 opened_at,cure_deadline,opened_at_ledger,deadline_ledger,status)
             VALUES (?,'BOB',500000000,800000000,
                     '2020-01-01T00:00:00Z','2020-01-01T01:00:00Z',1,1,'open')",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();

        let escalated = expire_overdue(&pool, &fast_policy(), 9999, &NoopLiquidationTrigger)
            .await.unwrap();
        assert_eq!(escalated, vec![id.clone()]);

        let status: String = sqlx::query_scalar(
            "SELECT status FROM margin_calls WHERE id=?",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "escalated");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn concurrent_cure_and_expire_exactly_one_wins() {
        let (pool, path) = test_pool().await;
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO margin_calls
                (id,wallet_address,deficit,required_top_up,
                 opened_at,cure_deadline,opened_at_ledger,deadline_ledger,status)
             VALUES (?,'CAROL',100000000,200000000,
                     '2020-01-01T00:00:00Z','2020-01-01T01:00:00Z',1,1,'open')",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();

        let snap = AccountMarginSnapshot {
            wallet_address: "CAROL".into(),
            equity: 1_000_000_000,
            initial_margin: 800_000_000,
            maintenance_margin: 600_000_000,
            ledger: 50,
        };

        let pa = pool.clone();
        let sa = snap.clone();
        let cure = tokio::spawn(async move {
            recheck(&pa, "CAROL", &sa, &fast_policy(), 50).await
        });
        let pb = pool.clone();
        let expire = tokio::spawn(async move {
            expire_overdue(&pb, &fast_policy(), 9999, &NoopLiquidationTrigger).await
        });

        let (cr, er) = tokio::join!(cure, expire);
        let cure_won = cr.unwrap().unwrap().is_some();
        let expire_won = !er.unwrap().unwrap().is_empty();

        let final_status: String = sqlx::query_scalar(
            "SELECT status FROM margin_calls WHERE id=?",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_ne!(final_status, "open");
        assert!(!(cure_won && expire_won), "both cannot win");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn cooldown_prevents_rapid_reopening() {
        let (pool, path) = test_pool().await;
        let policy = MarginCallPolicy {
            per_account_cooldown_secs: 300,
            ..MarginCallPolicy::default()
        };
        let deficit = MarginDeficit {
            wallet_address: "DAVE".into(),
            deficit: 100_000_000,
            required_top_up: 200_000_000,
            ledger: 1,
        };

        let mut tx = pool.begin().await.unwrap();
        open_call(&mut tx, &deficit, &policy).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx2 = pool.begin().await.unwrap();
        let err = open_call(&mut tx2, &deficit, &policy).await.unwrap_err();
        tx2.rollback().await.unwrap();

        assert!(matches!(err, MarginCallError::CooldownActive { .. }));

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }
}
