#![no_main]
use libfuzzer_sys::fuzz_target;
use zenith_backend::settlement_price::{determine, PriceObservation};
fuzz_target!(|prices: Vec<i64>| { let observations = prices.into_iter().take(256).enumerate().map(|(index, price)| PriceObservation { ledger: index as u32, price: price as i128, source: "fuzz".into() }).collect(); let _ = determine(observations, vec![], 0, 255, -1_000_000, 1_000_000, 10); });
