//! Integration tests for the indexer: gap detection, the historical backfill
//! and the idempotency of a replay.

mod common;

use std::sync::Arc;

use common::chain::{cleanup, indexer_pool};
use proptest::prelude::*;
use sqlx::SqlitePool;
use zenith_backend::chain::backfill::{plan_backfill, BackfillRunner, SkipReason};
use zenith_backend::chain::indexer::{
    detect_gaps, fetch_events, merge_ranges, project_events, ChainIndexer, IndexerConfig,
    LedgerGap, LedgerRange, ProjectionStore, RangeSource, SqliteProjectionStore,
};
use zenith_backend::chain::rpc::{MockSorobanRpc, SorobanRpc};
use zenith_backend::chain::types::GetEventsParams;

const FROM_LEDGER: u32 = 100;
const TO_LEDGER: u32 = 110;

fn mock_node() -> Arc<MockSorobanRpc> {
    let rpc = Arc::new(MockSorobanRpc::new(TO_LEDGER));
    rpc.with_ledger_events(FROM_LEDGER, TO_LEDGER);
    rpc
}

fn store(pool: &SqlitePool, stream: &str) -> Arc<SqliteProjectionStore> {
    Arc::new(SqliteProjectionStore::new(pool.clone(), stream))
}

/// The projected state of a stream, in a stable order.
async fn projection_state(pool: &SqlitePool, stream: &str) -> Vec<(String, i64, String, String)> {
    sqlx::query_as(
        "SELECT tx_hash, event_index, projection, payload FROM event_projections
         WHERE stream = ?1 ORDER BY tx_hash, event_index",
    )
    .bind(stream)
    .fetch_all(pool)
    .await
    .expect("the projection state")
}

#[test]
fn adjacent_and_overlapping_ranges_are_not_gaps() {
    let merged = merge_ranges(&[
        LedgerRange::new(100, 105),
        LedgerRange::new(105, 110),
        LedgerRange::new(108, 112),
    ]);
    assert_eq!(merged, vec![LedgerRange::new(100, 112)]);
    assert!(detect_gaps(&merged, 111).is_empty());
}

#[test]
fn a_hole_between_two_ranges_is_a_gap() {
    let gaps = detect_gaps(
        &[LedgerRange::new(100, 105), LedgerRange::new(106, 110)],
        110,
    );
    assert_eq!(gaps, vec![LedgerGap { from: 105, to: 106 }]);
}

#[tokio::test]
async fn a_dropped_range_is_replayed_into_an_identical_state() {
    let (pool, path) = indexer_pool("backfill").await;
    let rpc = mock_node();

    // The uninterrupted run: every ledger of the range is indexed.
    let complete = store(&pool, "complete");
    let events = fetch_events(
        &*rpc,
        &GetEventsParams {
            start_ledger: FROM_LEDGER,
            end_ledger: Some(TO_LEDGER),
            cursor: None,
            limit: Some(100),
            contract_ids: None,
        },
    )
    .await
    .expect("the events of the range");
    complete
        .write_events(&events)
        .await
        .expect("the events are written");
    complete
        .record_range(
            LedgerRange::new(FROM_LEDGER, TO_LEDGER + 1),
            RangeSource::Live,
            events.len() as u64,
        )
        .await
        .expect("the range is recorded");

    // The interrupted run: the range [105, 106) never made it.
    let interrupted = store(&pool, "interrupted");
    for (from, to) in [(FROM_LEDGER, 105u32), (106, TO_LEDGER + 1)] {
        let events = fetch_events(
            &*rpc,
            &GetEventsParams {
                start_ledger: from,
                end_ledger: Some(to - 1),
                cursor: None,
                limit: Some(100),
                contract_ids: None,
            },
        )
        .await
        .expect("the events of the range");
        interrupted
            .write_events(&events)
            .await
            .expect("the events are written");
        interrupted
            .record_range(
                LedgerRange::new(from, to),
                RangeSource::Live,
                events.len() as u64,
            )
            .await
            .expect("the range is recorded");
    }

    let gaps = detect_gaps(
        &interrupted.ranges().await.expect("the covered ranges"),
        TO_LEDGER,
    );
    assert_eq!(gaps, vec![LedgerGap { from: 105, to: 106 }]);

    // The repair replays the missing range through the same projection step.
    let plan = plan_backfill(&gaps, 0, 1_000);
    assert_eq!(plan.ranges, vec![LedgerRange::new(105, 106)]);

    let runner = BackfillRunner::new(
        Arc::clone(&rpc) as Arc<dyn SorobanRpc>,
        Arc::clone(&interrupted) as Arc<dyn ProjectionStore>,
    )
    .with_max_range(1_000)
    .with_rate_limit(std::time::Duration::ZERO);
    let report = runner.run(&plan).await.expect("the backfill");
    assert_eq!(report.ranges_replayed, 1);
    assert_eq!(report.events_replayed, 1);
    assert!(report.failures.is_empty());

    // The repaired state is identical to the uninterrupted one.
    let complete_state = projection_state(&pool, "complete").await;
    let repaired_state = projection_state(&pool, "interrupted").await;
    assert_eq!(complete_state.len(), 11);
    assert_eq!(complete_state, repaired_state);

    // The coverage is contiguous again.
    let ranges = merge_ranges(&interrupted.ranges().await.expect("the covered ranges"));
    assert_eq!(ranges, vec![LedgerRange::new(FROM_LEDGER, TO_LEDGER + 1)]);
    assert!(detect_gaps(&ranges, TO_LEDGER).is_empty());

    cleanup(path);
}

#[tokio::test]
async fn replaying_the_same_range_twice_changes_nothing() {
    let (pool, path) = indexer_pool("idempotent").await;
    let rpc = mock_node();
    let store = store(&pool, "idem");

    let runner = BackfillRunner::new(
        Arc::clone(&rpc) as Arc<dyn SorobanRpc>,
        Arc::clone(&store) as Arc<dyn ProjectionStore>,
    )
    .with_rate_limit(std::time::Duration::ZERO);
    let plan = plan_backfill(
        &[LedgerGap {
            from: FROM_LEDGER,
            to: TO_LEDGER + 1,
        }],
        0,
        1_000,
    );

    runner.run(&plan).await.expect("the first replay");
    let after_first = projection_state(&pool, "idem").await;
    let events_after_first = store.event_count().await.expect("the event count");

    runner.run(&plan).await.expect("the second replay");
    let after_second = projection_state(&pool, "idem").await;
    let events_after_second = store.event_count().await.expect("the event count");

    assert_eq!(after_first, after_second);
    assert_eq!(events_after_first, events_after_second);
    assert_eq!(after_first.len(), 11);

    cleanup(path);
}

#[tokio::test]
async fn a_range_outside_the_retention_window_is_reported_not_skipped() {
    let (pool, path) = indexer_pool("retention").await;
    let rpc = Arc::new(MockSorobanRpc::new(TO_LEDGER));
    rpc.with_ledger_events(FROM_LEDGER, TO_LEDGER);
    // The node only serves events from ledger 105 onwards.
    rpc.with_retention_floor(105);

    let store = store(&pool, "retention");
    let runner = BackfillRunner::new(
        Arc::clone(&rpc) as Arc<dyn SorobanRpc>,
        Arc::clone(&store) as Arc<dyn ProjectionStore>,
    )
    .with_rate_limit(std::time::Duration::ZERO);

    let floor = runner
        .detect_retention_floor(TO_LEDGER)
        .await
        .expect("the retention floor");
    assert_eq!(floor, 105);

    let plan = plan_backfill(
        &[LedgerGap {
            from: FROM_LEDGER,
            to: TO_LEDGER + 1,
        }],
        floor,
        1_000,
    );

    // The part of the gap the node still serves is replayed...
    assert_eq!(plan.ranges, vec![LedgerRange::new(105, TO_LEDGER + 1)]);
    // ...and the part it forgot is reported, not silently dropped.
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(
        plan.skipped[0].reason,
        SkipReason::OutsideRetention {
            oldest_available: 105
        }
    );

    cleanup(path);
}

#[tokio::test]
async fn the_gap_is_reported_as_a_readiness_signal() {
    let (pool, path) = indexer_pool("readiness").await;
    let rpc = mock_node();
    let store = store(&pool, "readiness");

    for (from, to) in [(FROM_LEDGER, 105u32), (106, TO_LEDGER + 1)] {
        store
            .record_range(LedgerRange::new(from, to), RangeSource::Live, 0)
            .await
            .expect("the range is recorded");
    }

    let indexer = ChainIndexer::new(
        Arc::clone(&rpc) as Arc<dyn SorobanRpc>,
        Arc::clone(&store) as Arc<dyn ProjectionStore>,
        IndexerConfig::default(),
    );

    let readiness = indexer.readiness().await;
    assert!(!readiness.ready);
    assert_eq!(readiness.open_gaps, 1);
    assert_eq!(readiness.gap_depth_ledgers, 1);
    assert!(readiness.reason.is_some());
    assert_eq!(readiness.tip, Some(TO_LEDGER));

    // The gap is also visible in the metrics.
    let snapshot = indexer.metrics().snapshot();
    assert_eq!(snapshot.gaps_detected, 1);
    assert_eq!(snapshot.gap_depth_ledgers, 1);

    cleanup(path);
}

#[tokio::test]
async fn the_backfill_updates_the_metrics() {
    let (pool, path) = indexer_pool("metrics").await;
    let rpc = mock_node();
    let store = store(&pool, "metrics");
    let runner = BackfillRunner::new(
        Arc::clone(&rpc) as Arc<dyn SorobanRpc>,
        Arc::clone(&store) as Arc<dyn ProjectionStore>,
    )
    .with_rate_limit(std::time::Duration::ZERO);

    let plan = plan_backfill(
        &[LedgerGap {
            from: FROM_LEDGER,
            to: TO_LEDGER + 1,
        }],
        0,
        1_000,
    );
    runner.run(&plan).await.expect("the replay");

    let snapshot = runner.metrics().snapshot();
    assert_eq!(snapshot.backfilled_ranges, 1);
    assert_eq!(snapshot.backfilled_ledgers, 11);
    assert_eq!(snapshot.backfilled_events, 11);
    assert_eq!(snapshot.replay_failures, 0);

    cleanup(path);
}

fn event_strategy() -> impl Strategy<Value = zenith_backend::chain::indexer::DecodedChainEvent> {
    (0u32..1_000, "[a-z0-9]{1,8}", -5i32..5, 0u32..4).prop_map(
        |(ledger, tx_hash, event_index, event_type)| {
            zenith_backend::chain::indexer::DecodedChainEvent {
                ledger,
                paging_token: format!("{ledger:08}-{event_index}"),
                tx_hash,
                event_index,
                contract_id: "contract".to_string(),
                event_type: match event_type {
                    0 => "OptionMinted".to_string(),
                    1 => "OptionExercised".to_string(),
                    2 => "CollateralDeposited".to_string(),
                    _ => "Unknown".to_string(),
                },
                payload: serde_json::json!({ "series_id": "XLM-CALL" }),
            }
        },
    )
}

proptest! {
    /// Projecting the same batch twice produces the same rows: a replay can
    /// never change the projection state.
    #[test]
    fn projecting_a_batch_twice_is_idempotent(
        events in prop::collection::vec(event_strategy(), 0..12)
    ) {
        let projected = project_events(&events);
        let once = zenith_backend::chain::indexer::dedupe_projections(projected.clone());
        let twice = zenith_backend::chain::indexer::dedupe_projections(projected);
        prop_assert_eq!(once, twice);
    }
}
