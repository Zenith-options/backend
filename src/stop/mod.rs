//! Route into the single auditable stop. `src/stop.rs` is the machine;
//! this module is the documented entry point for both the on-chain enforcement
//! hook and the off-chain audit sink. Application startup wires a single
//! `StopMachine` into both.

pub use crate::stop::event::StopEvent;
pub use crate::stop::Source;
pub use crate::stop::StopMode;
pub use crate::stop::{StopHook, StopLog, StopMachine};
