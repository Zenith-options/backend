pub mod decoder;

pub use decoder::{
    validate_decoded_event, DecodeError, DecodeOutcome, DecoderKey, DecoderRegistry,
    EventDecoder, FallbackStrategy, QuarantinedEvent, RawContractEvent,
};

use crate::analytics::realized_vol;

/// Notify analytics caches that new trades were indexed for `series`.
///
/// Called from the indexer write path after trades land so a backfill does
/// not serve a stale realized-volatility value.
pub fn on_trades_indexed(series: u64) {
    realized_vol::invalidate_series(series);
}
