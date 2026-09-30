//! On-chain / Soroban integration layer.
//!
//! This module hosts the internal Soroban RPC client used by every on-chain
//! feature (indexing, settlement, transaction building, reconciliation).
//! The client is intentionally hidden behind the [`rpc::SorobanRpc`] trait so
//! that callers depend on a stable surface and tests can substitute a mock.

pub mod bindings;
pub mod horizon;
pub mod readiness;
pub mod relayer;
pub mod rpc;
pub mod submit;
pub mod tx_builder;
pub mod types;

pub use horizon::{AccountResponse, BalanceLine, HorizonClient, HorizonError};
pub use readiness::{check_wallet_readiness, ReadinessItem, ReadinessReport};
pub use relayer::{FeeBumpRelayer, RelayerError, SponsorshipPolicy};
pub use rpc::{LedgerEntryInfo, SorobanRpcClient};
pub use submit::{SubmitError, TxSubmitter};
pub use 
pub mod horizon;
pub mod readiness;
pub mod relayer;
pub mod rpc;
pub mod submit;
pub mod tx_builder;
//! On-chain / Soroban integration layer.
//!
//! This module hosts the internal Soroban RPC client used by every on-chain
//! feature (indexing, settlement, transaction building, reconciliation).
//! The client is intentionally hidden behind the [`rpc::SorobanRpc`] trait so
//! that callers depend on a stable surface and tests can substitute a mock.

pub mod horizon;
pub mod readiness;
pub mod relayer;
pub mod rpc;
pub mod submit;
pub mod tx_builder;
pub mod types;

pub use horizon::{AccountResponse, BalanceLine, HorizonClient, HorizonError};
pub use readiness::{check_wallet_readiness, ReadinessItem, ReadinessReport};
pub use relayer::{FeeBumpRelayer, RelayerError, SponsorshipPolicy};
pub use rpc::{LedgerEntryInfo, RpcClient, RpcConfig, RpcError, SorobanRpc, SorobanRpcClient};
pub use submit::{SubmitError, TxSubmitter};
pub use tx_builder::{TxBuilder, TxBuilderError};
pub use types::{
    GetEventsRequest, GetEventsResponse, GetHealthResponse, GetLatestLedgerResponse,
    GetLedgerEntriesRequest, GetLedgerEntriesResponse, GetTransactionResponse,
    LedgerEntry, LedgerKey, SendTransactionRequest, SendTransactionResponse,
    SimulateTransactionRequest, SimulateTransactionResponse,
};
