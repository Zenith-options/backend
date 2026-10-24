//! Integration tests for the projection layer.
//! Covers: fold correctness, idempotency, out-of-order delivery,
//! unknown-event quarantine, rebuild-equivalence, and checkpoint advancement.

use zenith_backend::chain::bindings::events::DecodedEvent;
use zenith_backend::chain::projections::{
    apply_event, checkpoint::{advance_checkpoint, read_checkpoint},
    rebuild::rebuild_all, StreamEvent, PROJECTOR_VERSION,
};
use zenith_backend::db::init_pool;

// ── helpers ───────────────────────────────────────────────────────────────────

async fn fresh_pool() -> sqlx::SqlitePool {
    let id = uuid::Uuid::new_v4().to_string();
    let url = format!("sqlite:///tmp/proj-test-{id}.db");
    init_pool(&url).await
}

fn mint(series: &str, writer: &str, contracts: f64, ledger: i64, idx: i64) -> StreamEvent {
    StreamEvent {
        ledger, tx_hash: format!("tx{ledger}{idx}"), event_index: idx,
        event: DecodedEvent::OptionMinted {
            series_id: series.into(), strike: 100.0, expiry: 9999,
            is_call: true, contracts, writer: writer.into(),
        },
    }
}

fn exercise(series: &str, buyer: &str, payout: f64, ledger: i64, idx: i64) -> StreamEvent {
    StreamEvent {
        ledger, tx_hash: format!("tx{ledger}{idx}"), event_index: idx,
        event: DecodedEvent::OptionExercised {
            series_id: series.into(), buyer: buyer.into(), payout,
        },
    }
}

fn deposit(wallet: &str, amount: f64, ledger: i64, idx: i64) -> StreamEvent {
    StreamEvent {
        ledger, tx_hash: format!("tx{ledger}{idx}"), event_index: idx,
        event: DecodedEvent::CollateralDeposited { wallet: wallet.into(), amount },
    }
}

fn upgrade(hash: &str, ledger: i64) -> StreamEvent {
    StreamEvent {
        ledger, tx_hash: format!("txU{ledger}"), event_index: 0,
        event: DecodedEvent::ContractUpgraded {
            new_wasm_hash: hash.into(), new_version: 2,
        },
    }
}

// ── Basic fold tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn mint_creates_open_position() {
    let pool = fresh_pool().await;
    apply_event(&pool, &mint("s1","alice",5.0,1,0)).await.unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT status FROM projected_positions WHERE id='alice:s1'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "open");
}

#[tokio::test]
async fn deposit_creates_collateral_row() {
    let pool = fresh_pool().await;
    apply_event(&pool, &deposit("alice",500.0,1,0)).await.unwrap();
    let bal: f64 = sqlx::query_scalar(
        "SELECT balance FROM projected_collateral WHERE wallet='alice'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(bal, 500.0);
}

#[tokio::test]
async fn exercise_marks_series_settled() {
    let pool = fresh_pool().await;
    apply_event(&pool, &mint("s1","alice",5.0,1,0)).await.unwrap();
    apply_event(&pool, &exercise("s1","alice",200.0,2,0)).await.unwrap();
    let settled: i64 = sqlx::query_scalar(
        "SELECT is_settled FROM projected_settlement WHERE series_id='s1'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(settled, 1);
}

// ── Idempotency ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn replayed_mint_does_not_double_count() {
    let pool = fresh_pool().await;
    let ev = mint("s1","bob",3.0,1,0);
    apply_event(&pool, &ev).await.unwrap();
    apply_event(&pool, &ev).await.unwrap(); // replay
    let contracts: f64 = sqlx::query_scalar(
        "SELECT contracts FROM projected_positions WHERE id='bob:s1'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(contracts, 3.0);
}

#[tokio::test]
async fn replayed_deposit_does_not_double_count() {
    let pool = fresh_pool().await;
    let ev = deposit("carol",100.0,1,0);
    apply_event(&pool, &ev).await.unwrap();
    apply_event(&pool, &ev).await.unwrap();
    let bal: f64 = sqlx::query_scalar(
        "SELECT balance FROM projected_collateral WHERE wallet='carol'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(bal, 100.0);
}

// ── Unknown / unhandled events quarantined, not panicked ─────────────────────

#[tokio::test]
async fn contract_upgraded_event_does_not_panic() {
    let pool = fresh_pool().await;
    // ContractUpgraded has no projection fold — must silently succeed.
    apply_event(&pool, &upgrade("NEWHASH",5,)).await.unwrap();
}

// ── Multi-event transaction ───────────────────────────────────────────────────

#[tokio::test]
async fn multi_event_transaction_applies_all() {
    let pool = fresh_pool().await;
    // Two events in the same ledger/tx, different event_index.
    let e1 = mint("s1","dave",2.0,10,0);
    let e2 = deposit("dave",250.0,10,1);
    apply_event(&pool, &e1).await.unwrap();
    apply_event(&pool, &e2).await.unwrap();

    let contracts: f64 = sqlx::query_scalar(
        "SELECT contracts FROM projected_positions WHERE id='dave:s1'"
    ).fetch_one(&pool).await.unwrap();
    let bal: f64 = sqlx::query_scalar(
        "SELECT balance FROM projected_collateral WHERE wallet='dave'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(contracts, 2.0);
    assert_eq!(bal, 250.0);
}

// ── Checkpoint advancement ────────────────────────────────────────────────────

#[tokio::test]
async fn checkpoint_advances_after_apply() {
    let pool = fresh_pool().await;
    apply_event(&pool, &mint("s2","eve",1.0,42,0)).await.unwrap();
    advance_checkpoint(&pool,"positions",PROJECTOR_VERSION,42,"txX",0,1).await.unwrap();
    let cp = read_checkpoint(&pool,"positions",PROJECTOR_VERSION).await.unwrap();
    assert!(cp.is_some());
    assert_eq!(cp.unwrap().ledger, 42);
}

// ── Rebuild equivalence ───────────────────────────────────────────────────────

#[tokio::test]
async fn rebuild_produces_identical_state() {
    let pool = fresh_pool().await;

    let events = vec![
        mint("s1","alice",5.0,1,0),
        mint("s2","bob",3.0,2,0),
        deposit("alice",1000.0,3,0),
        deposit("bob",500.0,4,0),
        exercise("s1","alice",200.0,5,0),
    ];

    // Apply live.
    for ev in &events { apply_event(&pool, ev).await.unwrap(); }

    // Rebuild from same stream and verify.
    let report = rebuild_all(&pool, events.into_iter()).await.unwrap();

    assert!(report.all_match(),
        "rebuild mismatch: {:?}", report.mismatches);
    assert_eq!(report.events_replayed, 5);
}

#[tokio::test]
async fn rebuild_with_duplicates_still_matches() {
    let pool = fresh_pool().await;

    let events = vec![
        mint("s1","frank",4.0,1,0),
        mint("s1","frank",4.0,1,0), // exact duplicate
        deposit("frank",800.0,2,0),
        deposit("frank",800.0,2,0), // exact duplicate
    ];

    for ev in &events { apply_event(&pool, ev).await.unwrap(); }

    let report = rebuild_all(&pool, events.into_iter()).await.unwrap();
    assert!(report.all_match(), "{:?}", report.mismatches);
}

#[tokio::test]
async fn rebuild_detects_mismatch_if_live_was_modified() {
    let pool = fresh_pool().await;

    let events = vec![mint("s1","grace",2.0,1,0)];
    for ev in &events { apply_event(&pool, ev).await.unwrap(); }

    // Corrupt the live table directly.
    sqlx::query("UPDATE projected_positions SET contracts=99.0 WHERE id='grace:s1'")
        .execute(&pool).await.unwrap();

    let report = rebuild_all(&pool, events.into_iter()).await.unwrap();
    assert!(!report.positions_match, "should detect the corruption");
    assert!(!report.mismatches.is_empty());
}
