use crate::chain::xdr::ScValue;
pub fn deposit(owner: String, amount: i128) -> Vec<ScValue> { vec![ScValue::Address(owner), ScValue::I128(amount)] }
pub fn withdraw(owner: String, amount: i128) -> Vec<ScValue> { vec![ScValue::Address(owner), ScValue::I128(amount)] }
