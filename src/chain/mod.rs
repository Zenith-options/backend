//! On-chain / Soroban integration layer.
//!
//! This module hosts the internal Soroban RPC client used by every on-chain
//! feature (indexing, settlement, transaction building, reconciliation).
//! The client is intentionally hidden behind the [`rpc::SorobanRpc`] trait so
//! that callers depend on a stable surface and tests can substitute a mock.

pub mod rpc;
pub mod types;

pub use rpc::{RpcClient, RpcConfig, RpcError, SorobanRpc};
pub use types::{
    GetEventsRequest, GetEventsResponse, GetHealthResponse, GetLatestLedgerResponse,
    GetLedgerEntriesRequest, GetLedgerEntriesResponse, GetTransactionResponse,
    LedgerEntry, LedgerKey, SendTransactionRequest, SendTransactionResponse,
    SimulateTransactionRequest, SimulateTransactionResponse,
};
