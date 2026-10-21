//! Soroban chain-safety layer: protocol compatibility gating.
//!
//! Self-contained on purpose — it only depends on external crates, so it can
//! be wired into whichever chain/indexer runtime is active without dragging
//! in its internals.

pub mod jsonrpc;
pub mod protocol;
