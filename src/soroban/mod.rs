//! Soroban chain-safety layer: protocol compatibility gating, versioned
//! settlement event decoding and failed-invocation forensics.
//!
//! Self-contained on purpose — it only depends on external crates, so it can
//! be wired into whichever chain/indexer runtime is active without dragging
//! in its internals.

pub mod events;
pub mod forensics;
pub mod jsonrpc;
pub mod protocol;
