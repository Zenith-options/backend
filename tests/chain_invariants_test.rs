use std::collections::HashSet;
use zenith_backend::{settlement_price::{determine, PriceObservation}, settlement_saga::SettlementSaga};

#[test]
fn replay_does_not_double_apply_saga_steps() {
    let mut saga = SettlementSaga::new("workflow".into(), "key".into());
    saga.mark_applied(0, "hash".into(), 100).unwrap();
    saga.mark_applied(0, "hash".into(), 100).unwrap();
    assert_eq!(saga.steps.iter().filter(|step| step.transaction_hash.is_some()).count(), 1);
}

#[test]
fn settlement_observations_are_unique_and_bounded() {
    let observations = vec![PriceObservation { ledger: 10, price: 100, source: "primary".into() }, PriceObservation { ledger: 10, price: 100, source: "primary".into() }, PriceObservation { ledger: 11, price: 102, source: "primary".into() }];
    let settlement = determine(observations, vec![], 10, 11, 1, 1_000, 5).unwrap();
    let ledgers: HashSet<_> = settlement.observations.iter().map(|observation| observation.ledger).collect();
    assert_eq!(ledgers.len(), settlement.observations.len());
    assert_eq!(settlement.price, 101);
}
