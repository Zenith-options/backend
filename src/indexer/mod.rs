pub mod decoder;

pub use decoder::{
    validate_decoded_event, DecodeError, DecodeOutcome, DecoderKey, DecoderRegistry,
    EventDecoder, FallbackStrategy, QuarantinedEvent, RawContractEvent,
};
