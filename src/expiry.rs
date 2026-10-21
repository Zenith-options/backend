use std::cmp::Ordering;

/// Minimal expiry helper used by the position and settlement flows.
/// It keeps the app from panicking when a live position references an
/// absolute expiry timestamp but there is no dedicated expiry module yet.
pub fn snap_expiry(_underlying: &str, _now: i64, _days: i64) -> i64 {
    _now + _days.max(1) * 86_400
}

pub fn snap_expiry_for_value(_underlying: &str, value: &str) -> String {
    value.to_string()
}
