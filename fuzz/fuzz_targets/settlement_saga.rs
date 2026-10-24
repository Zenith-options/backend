#![no_main]
use libfuzzer_sys::fuzz_target;
use zenith_backend::settlement_saga::SettlementSaga;
fuzz_target!(|steps: Vec<u8>| { let mut saga = SettlementSaga::new("fuzz".into(), "key".into()); for step in steps { let index = step as usize % saga.steps.len(); let _ = saga.mark_applied(index, format!("hash-{step}"), step as u32); } });
