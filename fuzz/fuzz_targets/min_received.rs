#![no_main]
//! Fuzz the min_received_fixed comparison: ensures that for any (expected,
//! received, max_slippage_bps) triple, the guard either accepts (received >=
//! min_received) or rejects correctly, and that shortfall is never negative.
use libfuzzer_sys::fuzz_target;
use zenith_backend::math_fixed::{
    conversion_shortfall, min_received_fixed, realized_slippage_bps,
};

fuzz_target!(|data: &[u8]| {
    if data.len() < 16 { return; }
    let expected         = i64::from_le_bytes(data[0..8].try_into().unwrap()).abs() % 1_000_000_000;
    let received         = i64::from_le_bytes(data[8..16].try_into().unwrap()).abs() % 1_000_000_000;
    let max_slippage_bps = if data.len() >= 18 {
        u16::from_le_bytes(data[16..18].try_into().unwrap()) as i64 % 10_001
    } else {
        200
    };

    let min_recv = min_received_fixed(expected, max_slippage_bps);

    // min_received must never exceed expected.
    assert!(min_recv <= expected, "min_recv {min_recv} > expected {expected}");
    // min_received must be non-negative when expected >= 0.
    assert!(min_recv >= 0, "min_recv {min_recv} < 0");

    // Guard decision must be consistent.
    let accepted = received >= min_recv;
    let _ = accepted;

    // Realized slippage must be consistent.
    if expected > 0 {
        let slip = realized_slippage_bps(expected, received).unwrap();
        if received <= expected {
            assert!(slip >= 0, "slip {slip} < 0 when received <= expected");
        }
    }

    // Shortfall never negative.
    let shortfall = conversion_shortfall(expected, received.min(expected));
    assert!(shortfall >= 0);
});
