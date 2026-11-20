//! Liquidation subsystem: DEX/AMM routing fallback, auctions, and policy.
//!
//! The first-line mechanism is DEX/AMM routing (`liquidation_router.rs`).
//! When routing cannot clear collateral at an acceptable price, a sealed-bid
//! auction (`auction.rs`) lets external capital compete for the lot. A failed
//! auction falls back to risk-reserve coverage.

pub mod auction;
pub mod auction_api;
