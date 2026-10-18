use crate::chain::xdr::ScValue;
pub fn open_position(series_id: String, quantity: i128, owner: String) -> Vec<ScValue> { vec![ScValue::String(series_id), ScValue::I128(quantity), ScValue::Address(owner)] }
pub fn settle_position(position_id: String) -> Vec<ScValue> { vec![ScValue::String(position_id)] }
