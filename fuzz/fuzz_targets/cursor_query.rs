//! Fuzz target: cursor/pagination query-string decoding.
//!
//! `ListPositionsQuery` and `HistoryQuery` accept `?limit=&offset=`
//! query parameters via `serde_qs`/`serde_urlencoded`.  A malformed or
//! extreme value must never cause a panic; clamping logic is exercised
//! here to confirm it holds for all i64-range values.
//!
//! This target fuzzes the parsing logic independently of HTTP — it drives
//! `serde_urlencoded::from_str` directly, mirroring what axum's `Query`
//! extractor does under the hood.
#![no_main]

use libfuzzer_sys::fuzz_target;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ListPositionsQuery {
    status: Option<String>,
    strategy_id: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

const DEFAULT_LIST_LIMIT: i64 = 50;
const MAX_LIST_LIMIT: i64 = 200;

/// Replicates the offset validation from the list_positions/history handlers.
/// Negative offsets are rejected with a validation error (HTTP 400), matching
/// how non-integer values are handled; they are no longer clamped to zero.
fn validate_offset(offset: Option<i64>) -> Result<i64, &'static str> {
    match offset {
        Some(o) if o < 0 => Err("offset must be >= 0"),
        Some(o) => Ok(o),
        None => Ok(0),
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };

    // 1. ListPositionsQuery — must not panic
    if let Ok(q) = serde_urlencoded::from_str::<ListPositionsQuery>(s) {
        // Replicate the clamping logic from list_positions handler
        let limit = q.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);

        // Clamped limit must always be in [1, MAX_LIST_LIMIT]
        assert!(
            limit >= 1 && limit <= MAX_LIST_LIMIT,
            "limit={limit} out of bounds after clamp"
        );

        // Negative offsets must be rejected, not clamped.
        match validate_offset(q.offset) {
            Ok(offset) => assert!(
                offset >= 0,
                "offset={offset} should be >= 0 after validation"
            ),
            Err(_) => assert!(
                q.offset.map(|o| o < 0).unwrap_or(false),
                "offset rejected but was not negative"
            ),
        }
    }

    // 2. HistoryQuery — same treatment
    if let Ok(q) = serde_urlencoded::from_str::<HistoryQuery>(s) {
        let limit = q.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
        assert!(limit >= 1 && limit <= MAX_LIST_LIMIT);

        match validate_offset(q.offset) {
            Ok(offset) => assert!(offset >= 0),
            Err(_) => assert!(q.offset.map(|o| o < 0).unwrap_or(false)),
        }
    }
});
