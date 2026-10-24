use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use zenith_backend::soroban::events::*;

const G: &str = "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN7";
const C: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";

async fn pool() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    ensure_schema(&pool).await.unwrap();
    pool
}

fn ev(id: &str, ledger: u32, version: u32, data: serde_json::Value) -> RawContractEvent {
    RawContractEvent {
        id: id.into(),
        contract_id: C.into(),
        ledger,
        tx_hash: format!("tx{ledger}"),
        topics: vec![json!("zenith"), json!("series_settled"), json!(version)],
        data,
    }
}

/// A registry that only knows v1 — i.e. the backend before the contract upgrade.
fn v1_only() -> SchemaDecoderRegistry {
    let schemas: Vec<EventSchema> = SETTLEMENT_SCHEMAS
        .iter()
        .filter(|s| s.event == "series_settled" && s.version == 1)
        .copied()
        .collect();
    let schemas: &'static [EventSchema] = Box::leak(schemas.into_boxed_slice());
    SchemaDecoderRegistry::build(
        schemas,
        &[("series_settled", 1, |f| SettlementEvent::SeriesSettled {
            series_id: f.str("series_id"),
            settlement_price: f.i128("settlement_price"),
            price_decimals: V1_PRICE_DECIMALS,
            settled_at: f.u64("settled_at"),
            oracle: None,
        })],
    )
    .unwrap()
}

#[tokio::test]
async fn contract_upgrade_is_quarantined_then_replayed_not_dropped() {
    let db = pool().await;
    let v2_body = json!({
        "series_id": "XLM_C_0_12", "settlement_price": "1182000", "price_decimals": 7,
        "settled_at": 1_790_000_000u64, "oracle": C
    });

    // Old backend: v1 decodes, v2 (from the upgraded contract) is quarantined.
    let old = EventIngestor::new(v1_only(), db.clone());
    let v1 = ev(
        "e1",
        10,
        1,
        json!({"series_id": "XLM_P_0_10", "settlement_price": 1, "settled_at": 5}),
    );
    assert!(matches!(
        old.ingest(&v1).await.unwrap(),
        IngestOutcome::Decoded(_)
    ));
    let out = old.ingest(&ev("e2", 20, 2, v2_body.clone())).await.unwrap();
    assert!(matches!(
        out,
        IngestOutcome::Quarantined {
            failure: DecodeFailure::UnknownVersion { version: 2, .. },
            ..
        }
    ));
    // Seeing it again (restart / re-scan) doesn't duplicate it.
    old.ingest(&ev("e2", 20, 2, v2_body.clone())).await.unwrap();

    let summary = old.quarantine_summary().await.unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].reason, "unknown_version");
    assert_eq!(summary[0].schema_version, Some(2));
    assert_eq!(summary[0].count, 1);
    assert_eq!(old.earliest_quarantined_ledger().await.unwrap(), Some(20));

    // Old backend replay: still unknown, stays quarantined, attempts counted.
    assert!(old.replay_quarantine().await.unwrap().is_empty());
    let (attempts,): (i64,) =
        sqlx::query_as("SELECT attempts FROM event_quarantine WHERE event_id = 'e2'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(attempts, 3);

    // New backend ships the v2 decoder: the backlog decodes and is released.
    let new = EventIngestor::new(SchemaDecoderRegistry::settlement(), db.clone());
    let released = new.replay_quarantine().await.unwrap();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].event_id, "e2");
    assert_eq!(
        released[0].event,
        SettlementEvent::SeriesSettled {
            series_id: "XLM_C_0_12".into(),
            settlement_price: 1_182_000,
            price_decimals: 7,
            settled_at: 1_790_000_000,
            oracle: Some(C.into()),
        }
    );
    assert!(new.quarantine_summary().await.unwrap().is_empty());
    assert_eq!(new.earliest_quarantined_ledger().await.unwrap(), None);
}

#[tokio::test]
async fn schema_violations_and_foreign_events() {
    let db = pool().await;
    let ing = EventIngestor::new(SchemaDecoderRegistry::settlement(), db.clone());

    let foreign = RawContractEvent {
        topics: vec![json!("transfer"), json!(G)],
        ..ev("x", 1, 1, json!(1))
    };
    assert_eq!(ing.ingest(&foreign).await.unwrap(), IngestOutcome::Ignored);

    // Same version, but the contract added a field without bumping it.
    let sneaky = ev(
        "e3",
        30,
        1,
        json!({"series_id": "S", "settlement_price": 1, "settled_at": 1, "extra": 1}),
    );
    assert!(matches!(
        ing.ingest(&sneaky).await.unwrap(),
        IngestOutcome::Quarantined {
            failure: DecodeFailure::SchemaViolation { .. },
            ..
        }
    ));
    let (reason, data): (String, String) =
        sqlx::query_as("SELECT reason, data FROM event_quarantine WHERE event_id = 'e3'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(reason, "schema_violation");
    assert!(data.contains("extra"), "raw payload is preserved");
}
