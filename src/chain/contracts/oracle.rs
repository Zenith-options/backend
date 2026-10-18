use crate::chain::xdr::ScValue;
pub fn publish(underlying: String, price: i128, volatility: i128, timestamp: i64) -> Vec<ScValue> { vec![ScValue::Symbol(underlying), ScValue::I128(price), ScValue::I128(volatility), ScValue::I128(timestamp as i128)] }
