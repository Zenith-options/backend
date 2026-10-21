//! Soroban chain integration.
//!
//! * [`rpc`] — the JSON-RPC client and the trait the rest of the platform
//!   talks to a node through.
//! * [`simulate`] — transaction simulation: ledger footprint, declared
//!   resources and typed simulation errors.
//! * [`fees`] — the dynamic inclusion-fee strategy and the rolling daily
//!   budget.
//! * [`tx_builder`] — envelope assembly: simulate, price, assemble.
//! * [`indexer`] / [`backfill`] — event ingestion, gap detection and the
//!   historical replay.
//!
//! `oracle_publisher` is not wired in here: it still targets the Postgres
//! era of the code base (it depends on `chrono`, `thiserror` and the
//! Postgres-only settlement module, none of which are part of the crate).

pub mod backfill;
pub mod bindings;
pub mod errors;
pub mod fees;
pub mod horizon;
pub mod indexer;
pub mod readiness;
pub mod relayer;
pub mod rpc;
pub mod simulate;
pub mod submit;
pub mod tx_builder;
pub mod types;

pub use backfill::{
    plan_backfill, BackfillPlan, BackfillReport, BackfillRunner, SkipReason, SkippedRange,
};
pub use errors::{
    map_contract_error, map_host_error, map_host_error_code, ApiErrorCode, DecodedError,
    HostErrorClass,
};
pub use fees::{
    DailyFeeBudget, FeeOracle, FeeOracleError, FeeSource, FeeStats, InclusionFee,
    InclusionFeeStrategy, RpcFeeOracle, StaticFeeOracle,
};
pub use horizon::{AccountResponse, BalanceLine, HorizonClient, HorizonError};
pub use indexer::{
    ChainIndexer, IndexerConfig, IndexerError, IndexerMetrics, IndexerMetricsSnapshot,
    IndexerReadiness, LedgerGap, LedgerRange, ProjectionStore, RangeSource, SqliteProjectionStore,
};
pub use readiness::{check_wallet_readiness, ReadinessItem, ReadinessReport};
pub use relayer::{FeeBumpRelayer, RelayerError, SponsorshipPolicy};
pub use rpc::{LedgerEntryInfo, MockSorobanRpc, RpcClient, RpcError, SorobanRpc, SorobanRpcClient};
pub use simulate::{
    footprint_hash, resource_fee_for, ResourcePricing, SimulatedTransaction, SimulationError,
    SorobanSimulator,
};
pub use submit::{SubmitError, TxSubmitter};
pub use tx_builder::{
    BuiltTransaction, SorobanTxBuilder, TransactionEnvelope, TxBuildRequest, TxBuilder,
    TxBuilderError,
};
pub use types::{
    GetEventsParams, GetEventsResponse, GetFeeStatsResponse, SimulateFootprint, SimulateResources,
    SimulateTransactionResponse,
};
