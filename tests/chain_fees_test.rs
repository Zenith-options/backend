//! Integration tests for the Soroban fee path: footprint extraction, the
//! resource fee, the dynamic inclusion fee and the daily budget.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::chain::{apply_schema, cleanup};
use zenith_backend::chain::fees::{
    DailyFeeBudget, FeeOracle, FeeSource, FeeStats, InclusionFeeStrategy, RpcFeeOracle,
    StaticFeeOracle,
};
use zenith_backend::chain::rpc::{MockSorobanRpc, RpcClient, SorobanRpc};
use zenith_backend::chain::simulate::{
    resource_fee_for, ResourcePricing, SimulationError, SorobanSimulator,
};
use zenith_backend::chain::tx_builder::{
    record_fee_audit, BuiltTransaction, SorobanTxBuilder, TxBuildRequest,
};
use zenith_backend::chain::types::{GetFeeStatsResponse, SimulateTransactionResponse};

const FEE_STATS_FIXTURE: &str = include_str!("fixtures/fee_stats.sample.json");
const SIMULATION_FIXTURE: &str = include_str!("fixtures/simulation.sample.json");

fn recorded_fee_stats() -> GetFeeStatsResponse {
    serde_json::from_str(FEE_STATS_FIXTURE).expect("the recorded fee statistics parse")
}

fn recorded_simulation() -> SimulateTransactionResponse {
    serde_json::from_str(SIMULATION_FIXTURE).expect("the recorded simulation parses")
}

fn request() -> TxBuildRequest {
    TxBuildRequest {
        source_account: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
        sequence: 4_821_337,
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM".into(),
        function: "deposit".into(),
        args: vec!["10000000".into()],
        valid_until_ledger: Some(4_821_350),
    }
}

/// A builder backed by a scripted node.
fn builder(rpc: Arc<MockSorobanRpc>, stats: Option<GetFeeStatsResponse>) -> SorobanTxBuilder {
    let oracle: Arc<dyn FeeOracle> = match stats {
        Some(stats) => Arc::new(StaticFeeOracle::new(stats.into())),
        None => Arc::new(StaticFeeOracle::unavailable()),
    };
    SorobanTxBuilder::new(rpc, oracle, InclusionFeeStrategy::default())
}

#[test]
fn the_recorded_sample_drives_the_bid() {
    let stats = FeeStats::from(recorded_fee_stats());
    let strategy = InclusionFeeStrategy::default();

    let fee = strategy
        .select(10_000, Some(&stats))
        .expect("a fee is selected");
    assert_eq!(fee.amount_stroops, 5_000, "the p90 of the sample is bid");
    assert_eq!(fee.source, FeeSource::Percentile);
    assert_eq!(fee.percentile, Some(90));
    assert_eq!(fee.total_stroops(), 15_000);
}

#[test]
fn the_recorded_simulation_yields_a_footprint_and_a_resource_fee() {
    let simulated = SorobanSimulator::interpret(recorded_simulation()).expect("a simulation");
    let footprint = simulated.footprint().expect("a footprint");
    assert_eq!(footprint.read.len(), 2);
    assert_eq!(footprint.write.len(), 1);
    assert_eq!(simulated.min_resource_fee, 125_000);

    // The resource fee follows the declared resources, not a constant.
    let pricing = ResourcePricing::default();
    assert_eq!(
        resource_fee_for(&simulated.resources, simulated.min_resource_fee, &pricing),
        148_392 + 5_120 + 1_024 + 2
    );
}

#[tokio::test]
async fn a_built_transaction_carries_the_footprint_and_both_fees() {
    let rpc = Arc::new(MockSorobanRpc::new(4_821_337));
    rpc.with_simulation(Ok(recorded_simulation()));
    let builder = builder(Arc::clone(&rpc), Some(recorded_fee_stats()));

    let built = builder
        .build(&request(), None)
        .await
        .expect("a transaction");
    assert_eq!(built.read_entries, 2);
    assert_eq!(built.write_entries, 1);
    assert_eq!(built.footprint_hash.len(), 64);
    assert_eq!(built.resource_fee_stroops, 154_538);
    assert_eq!(built.inclusion_fee.amount_stroops, 5_000);
    assert_eq!(built.simulated_ledger, 4_821_337);
    assert!(!built.envelope_xdr.is_empty());
}

#[tokio::test]
async fn the_floor_is_bid_when_the_node_has_no_fee_statistics() {
    let rpc = Arc::new(MockSorobanRpc::new(4_821_337));
    rpc.with_simulation(Ok(recorded_simulation()));
    // A node that does not implement `getFeeStats`.
    rpc.with_fee_stats(Err(
        zenith_backend::chain::rpc::RpcError::MethodUnavailable("getFeeStats".into()),
    ));

    let oracle = Arc::new(RpcFeeOracle::new(Arc::clone(&rpc) as Arc<dyn SorobanRpc>));
    let builder = SorobanTxBuilder::new(
        Arc::clone(&rpc) as Arc<dyn SorobanRpc>,
        oracle,
        InclusionFeeStrategy::default(),
    );

    let built = builder
        .build(&request(), None)
        .await
        .expect("a transaction");
    assert_eq!(built.inclusion_fee.amount_stroops, 100);
    assert_eq!(built.inclusion_fee.source, FeeSource::Floor);
}

#[tokio::test]
async fn the_daily_budget_blocks_a_submission() {
    let rpc = Arc::new(MockSorobanRpc::new(4_821_337));
    rpc.with_simulation(Ok(recorded_simulation()));
    let builder = builder(Arc::clone(&rpc), Some(recorded_fee_stats()));

    let budget = DailyFeeBudget::new(1_000);
    let err = builder
        .build(&request(), Some(&budget))
        .await
        .expect_err("the budget is exhausted");
    assert!(matches!(
        err,
        zenith_backend::chain::tx_builder::TxBuilderError::FeeStrategy(
            zenith_backend::chain::fees::FeeStrategyError::BudgetExceeded { .. }
        )
    ));

    // Nothing was reserved by the refused transaction.
    assert_eq!(budget.spent(), 0);
}

#[tokio::test]
async fn the_per_transaction_cap_reduces_the_bid() {
    let rpc = Arc::new(MockSorobanRpc::new(4_821_337));
    rpc.with_simulation(Ok(recorded_simulation()));
    let builder = SorobanTxBuilder::new(
        Arc::clone(&rpc) as Arc<dyn SorobanRpc>,
        Arc::new(StaticFeeOracle::new(recorded_fee_stats().into())),
        InclusionFeeStrategy {
            per_transaction_cap_stroops: 155_000,
            ..Default::default()
        },
    );

    let built = builder
        .build(&request(), None)
        .await
        .expect("a transaction");
    // The resource fee alone is 154 538, so only 462 stroops are left.
    assert_eq!(built.inclusion_fee.amount_stroops, 462);
    assert_eq!(built.inclusion_fee.source, FeeSource::Capped);
}

#[tokio::test]
async fn an_archived_entry_is_reported_as_a_typed_error() {
    let rpc = Arc::new(MockSorobanRpc::new(4_821_337));
    let mut simulation = recorded_simulation();
    let mut resources = simulation.resources.clone().expect("resources");
    resources.restore_footprint_required = true;
    resources.restore_footprint = resources.footprint.clone();
    simulation.resources = Some(resources);
    rpc.with_simulation(Ok(simulation));
    let builder = builder(Arc::clone(&rpc), Some(recorded_fee_stats()));

    let err = builder
        .build(&request(), None)
        .await
        .expect_err("a typed error");
    match err {
        zenith_backend::chain::tx_builder::TxBuilderError::Simulation(
            SimulationError::ArchivedEntries { entries, .. },
        ) => assert_eq!(entries.len(), 3),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn the_fee_audit_is_persisted_with_the_transaction() {
    let (pool, path) = common::chain::indexer_pool("fee-audit").await;
    // The transaction record itself, plus the audit migration.
    sqlx::query(
        "CREATE TABLE chain_txs (
             hash TEXT PRIMARY KEY,
             wallet TEXT NOT NULL,
             kind TEXT NOT NULL,
             status TEXT NOT NULL DEFAULT 'PENDING',
             submitted_at TEXT,
             ledger BIGINT,
             result_xdr TEXT,
             error TEXT
         )",
    )
    .execute(&pool)
    .await
    .expect("the transaction table");
    apply_schema(&pool, include_str!("../migrations/0013_chain_tx_fees.sql")).await;

    sqlx::query("INSERT INTO chain_txs (hash, wallet, kind) VALUES (?1, ?2, ?3)")
        .bind("tx-hash")
        .bind("wallet")
        .bind("deposit")
        .execute(&pool)
        .await
        .expect("the transaction record");

    let rpc = Arc::new(MockSorobanRpc::new(4_821_337));
    rpc.with_simulation(Ok(recorded_simulation()));
    let builder = builder(Arc::clone(&rpc), Some(recorded_fee_stats()));
    let built: BuiltTransaction = builder
        .build(&request(), None)
        .await
        .expect("a transaction");

    record_fee_audit(&pool, "tx-hash", &built)
        .await
        .expect("the audit is recorded");

    let row: (Option<i64>, Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT inclusion_fee, resource_fee, footprint_hash, fee_source FROM chain_txs WHERE hash = ?1",
    )
    .bind("tx-hash")
    .fetch_one(&pool)
    .await
    .expect("the audited transaction");
    assert_eq!(row.0, Some(5_000));
    assert_eq!(row.1, Some(154_538));
    assert_eq!(row.2, Some(built.footprint_hash.clone()));
    assert_eq!(row.3, Some("percentile".to_string()));

    let audit: (i64, i64, String) = sqlx::query_as(
        "SELECT inclusion_fee, resource_fee, footprint_hash FROM chain_tx_fees WHERE tx_hash = ?1",
    )
    .bind("tx-hash")
    .fetch_one(&pool)
    .await
    .expect("the audit row");
    assert_eq!(audit.0, 5_000);
    assert_eq!(audit.1, 154_538);
    assert_eq!(audit.2, built.footprint_hash);

    cleanup(path);
}

#[tokio::test]
async fn a_transaction_built_by_this_path_is_accepted_by_the_sandbox() {
    let url = std::env::var("ZENITH_SANDBOX_RPC_URL")
        .unwrap_or_else(|_| "http://localhost:8000/soroban/rpc".to_string());
    let rpc = RpcClient::new(url.clone()).with_timeout(Duration::from_secs(5));

    // The sandbox is optional: the test asserts the behaviour of the fee
    // path, not the availability of a node.
    if rpc.latest_ledger().await.is_err() {
        eprintln!("skipping: no local sandbox at {url}");
        return;
    }

    let oracle = Arc::new(RpcFeeOracle::new(Arc::new(rpc.clone())));
    let builder = SorobanTxBuilder::new(
        Arc::new(rpc.clone()) as Arc<dyn SorobanRpc>,
        oracle,
        InclusionFeeStrategy::default(),
    );

    let built = builder.build(&request(), None).await;
    let built = match built {
        Ok(built) => built,
        Err(err) => {
            // A node that does not report the footprint is refused with a
            // typed error: the platform never signs an envelope without a
            // footprint.
            assert!(matches!(
                err,
                zenith_backend::chain::tx_builder::TxBuilderError::Simulation(_)
            ));
            eprintln!("skipping: the sandbox does not report a footprint");
            return;
        }
    };

    assert!(built.footprint_hash.len() == 64);
    assert!(built.inclusion_fee.amount_stroops > 0);

    let submitted = rpc.send_transaction(&built.envelope_xdr).await;
    match submitted {
        Ok(response) => assert!(
            matches!(
                response.status.as_str(),
                "PENDING" | "DUPLICATE" | "TRY_AGAIN_LATER"
            ),
            "the sandbox accepted the transaction: {}",
            response.status
        ),
        Err(err) => {
            // The node has to accept the envelope; a transport or decode
            // failure means the envelope is not something we can send.
            panic!("the sandbox rejected the envelope: {err}");
        }
    }
}
