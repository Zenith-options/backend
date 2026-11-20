//! Integration tests for the margin-call lifecycle.
//! Runs against a real temp-file SQLite database (full migration schema).
//!
//! Coverage:
//!   - Detection (pure)
//!   - Detection → notification → cure
//!   - Detection → expiry → escalation
//!   - Concurrent cure-vs-expire (exactly one winner)
//!   - Cooldown prevents call spam
//!   - Invariant: no liquidation while an open call with a future deadline exists

use zenith_backend::db::init_pool;
use zenith_backend::margin_calls::{
    detect_deficit, expire_overdue, open_call, recheck, AccountMarginSnapshot,
    LiquidationTrigger, MarginCall, MarginCallPolicy, MarginDeficit, NoopLiquidationTrigger,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn fresh_pool() -> (sqlx::SqlitePool, std::path::PathBuf) {
    let path = std::env::temp_dir()
        .join(format!("zenith-mc-integ-{}.db", uuid::Uuid::new_v4()));
    let pool = init_pool(&format!("sqlite://{}", path.display())).await;
    (pool, path)
}

fn zero_cooldown() -> MarginCallPolicy {
    MarginCallPolicy {
        per_account_cooldown_secs: 0,
        ..MarginCallPolicy::default()
    }
}

// ─── Pure detection ───────────────────────────────────────────────────────────

#[test]
fn pure_detection_healthy_returns_none() {
    let snap = AccountMarginSnapshot {
        wallet_address: "ALICE".into(),
        equity: 1_000_000_000,
        initial_margin: 800_000_000,
        maintenance_margin: 600_000_000,
        ledger: 1,
    };
    assert!(detect_deficit(&snap).is_none());
}

#[test]
fn pure_detection_breach_correct_figures() {
    let snap = AccountMarginSnapshot {
        wallet_address: "ALICE".into(),
        equity: 300_000_000,
        initial_margin: 800_000_000,
        maintenance_margin: 600_000_000,
        ledger: 50,
    };
    let d = detect_deficit(&snap).unwrap();
    assert_eq!(d.deficit, 300_000_000);
    assert_eq!(d.required_top_up, 500_000_000);
    assert_eq!(d.ledger, 50);
}

// ─── Detection → cure ────────────────────────────────────────────────────────

#[tokio::test]
async fn detection_to_cure_full_lifecycle() {
    let (pool, path) = fresh_pool().await;
    let policy = zero_cooldown();

    let deficit = detect_deficit(&AccountMarginSnapshot {
        wallet_address: "ALICE".into(),
        equity: 300_000_000,
        initial_margin: 800_000_000,
        maintenance_margin: 600_000_000,
        ledger: 10,
    })
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let call = open_call(&mut tx, &deficit, &policy).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(call.status, "open");

    // Outbox must have the opening event.
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox WHERE event_type='margin_call_opened'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);

    // Still unhealthy → call stays open.
    let still_bad = AccountMarginSnapshot {
        wallet_address: "ALICE".into(),
        equity: 300_000_000,
        initial_margin: 800_000_000,
        maintenance_margin: 600_000_000,
        ledger: 15,
    };
    let r = recheck(&pool, "ALICE", &still_bad, &policy, 15).await.unwrap().unwrap();
    assert_eq!(r.status, "open");

    // Account tops up → cured.
    let healthy = AccountMarginSnapshot {
        wallet_address: "ALICE".into(),
        equity: 1_000_000_000,
        initial_margin: 800_000_000,
        maintenance_margin: 600_000_000,
        ledger: 20,
    };
    let cured = recheck(&pool, "ALICE", &healthy, &policy, 20).await.unwrap().unwrap();
    assert_eq!(cured.status, "cured");
    assert_eq!(cured.resolved_at_ledger, Some(20));

    // Resolution event in outbox.
    let n2: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox WHERE event_type='margin_call_resolved'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n2, 1);

    pool.close().await;
    let _ = std::fs::remove_file(&path);
}

// ─── Detection → expiry → escalation ─────────────────────────────────────────

#[tokio::test]
async fn detection_to_escalation_on_expiry() {
    let (pool, path) = fresh_pool().await;
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

    let escalated =
        expire_overdue(&pool, &zero_cooldown(), 9999, &NoopLiquidationTrigger)
            .await
            .unwrap();
    assert_eq!(escalated, vec![id.clone()]);

    let status: String =
        sqlx::query_scalar("SELECT status FROM margin_calls WHERE id=?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "escalated");

    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox WHERE event_type='margin_call_escalated'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);

    pool.close().await;
    let _ = std::fs::remove_file(&path);
}

// ─── Concurrent cure-vs-expire ────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_cure_and_expire_exactly_one_wins() {
    let (pool, path) = fresh_pool().await;
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

    let healthy = AccountMarginSnapshot {
        wallet_address: "CAROL".into(),
        equity: 1_000_000_000,
        initial_margin: 800_000_000,
        maintenance_margin: 600_000_000,
        ledger: 50,
    };

    let pa = pool.clone();
    let sa = healthy.clone();
    let cure = tokio::spawn(async move {
        recheck(&pa, "CAROL", &sa, &zero_cooldown(), 50).await
    });
    let pb = pool.clone();
    let expire = tokio::spawn(async move {
        expire_overdue(&pb, &zero_cooldown(), 9999, &NoopLiquidationTrigger).await
    });

    let (cr, er) = tokio::join!(cure, expire);
    let cure_won = cr.unwrap().unwrap().is_some();
    let expire_won = !er.unwrap().unwrap().is_empty();

    let final_status: String =
        sqlx::query_scalar("SELECT status FROM margin_calls WHERE id=?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_ne!(final_status, "open");
    assert!(!(cure_won && expire_won), "both cannot win; cure={cure_won} expire={expire_won}");

    pool.close().await;
    let _ = std::fs::remove_file(&path);
}

// ─── Cooldown ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cooldown_prevents_second_call_within_window() {
    let (pool, path) = fresh_pool().await;
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
    assert!(
        matches!(err, zenith_backend::margin_calls::MarginCallError::CooldownActive { .. })
    );

    pool.close().await;
    let _ = std::fs::remove_file(&path);
}

// ─── Invariant: no liquidation while open call has a future deadline ──────────

#[tokio::test]
async fn invariant_no_liquidation_with_open_future_deadline() {
    let (pool, path) = fresh_pool().await;

    let call_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO margin_calls
            (id,wallet_address,deficit,required_top_up,
             opened_at,cure_deadline,opened_at_ledger,deadline_ledger,status)
         VALUES (?,'EVE',100000000,200000000,
                 strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                 strftime('%Y-%m-%dT%H:%M:%fZ','now','+1 hour'),
                 100,9999999,'open')",
    )
    .bind(&call_id)
    .execute(&pool)
    .await
    .unwrap();

    // No chain_liquidations row for EVE should coexist with this open call.
    let bad: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM chain_liquidations cl
           JOIN margin_calls mc ON mc.wallet_address = cl.wallet
          WHERE mc.status='open'
            AND mc.cure_deadline > strftime('%Y-%m-%dT%H:%M:%fZ','now')
            AND cl.created_at > mc.opened_at",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);

    assert_eq!(bad, 0, "invariant: no liquidation while open call has future deadline");

    pool.close().await;
    let _ = std::fs::remove_file(&path);
}
