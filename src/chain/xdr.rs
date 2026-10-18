use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const STROOP_SCALE: i128 = 10_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScValue {
    I128(i128),
    Symbol(String),
    String(String),
    Address(String),
    Vec(Vec<ScValue>),
    Map(BTreeMap<String, ScValue>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XdrDecodeError { WrongType(&'static str), InvalidAddress, MissingField(String), Overflow }

pub fn minor_units_to_stroops(value: i128, source_decimals: u32) -> Result<i128, XdrDecodeError> {
    if source_decimals > 7 { return Err(XdrDecodeError::WrongType("precision exceeds Stellar's seven decimals")); }
    value.checked_mul(10_i128.pow(7 - source_decimals)).ok_or(XdrDecodeError::Overflow)
}

pub fn stroops(value: &ScValue) -> Result<i128, XdrDecodeError> {
    match value { ScValue::I128(amount) => Ok(*amount), _ => Err(XdrDecodeError::WrongType("i128")) }
}

pub fn address(value: &ScValue) -> Result<&str, XdrDecodeError> {
    match value { ScValue::Address(address) if (address.starts_with('G') || address.starts_with('C')) && address.len() == 56 => Ok(address), ScValue::Address(_) => Err(XdrDecodeError::InvalidAddress), _ => Err(XdrDecodeError::WrongType("address")) }
}

pub fn required<'a>(map: &'a BTreeMap<String, ScValue>, key: &str) -> Result<&'a ScValue, XdrDecodeError> {
    map.get(key).ok_or_else(|| XdrDecodeError::MissingField(key.into()))
}
