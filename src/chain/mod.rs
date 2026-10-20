//! On-chain interaction: Soroban RPC types, Horizon client, transaction
//! simulation/submission, and the Merkle-commitment batch settlement logic
//! (issue #196).
//!
//! Only the subset of the chain surface that the compiled backend needs is
//! declared here. Modules that depend on the quarantined indexer/relayer
//! feature set (`readiness`, `relayer`, `rpc`, `tx_builder`, `oracle_publisher`)
//! are intentionally omitted; their behaviour is out of scope for #196. The
//! `batch` and `merkle` modules are self-contained and pure so an independent
//! verifier can reproduce them exactly.

pub mod batch;
pub mod bindings;
pub mod errors;
pub mod horizon;
pub mod merkle;
pub mod settlement;
pub mod simulate;
pub mod submit;
pub mod types;

pub use batch::{
    check_conservation, root_hex, BatchError, BatchStatus, LeafView, PlannedBatch,
    SettlementInstruction,
};
pub use horizon::{AccountResponse, BalanceLine, HorizonClient, HorizonError};
pub use merkle::{
    Commitment, Hash, MerkleError, MerkleProof, MerkleTree, SettlementLeaf, LEAF_VERSION,
};
