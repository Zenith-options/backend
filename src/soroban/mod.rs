//! Soroban chain-safety layer: protocol compatibility gating and versioned
//! settlement event decoding.
//!
//! Self-contained on purpose — it only depends on external crates, so it can
//! be wired into whichever chain/indexer runtime is active without dragging
//! in its internals.

pub mod events;
pub mod jsonrpc;
pub mod protocol;
