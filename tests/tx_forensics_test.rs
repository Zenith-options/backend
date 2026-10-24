use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;
use zenith_backend::soroban::forensics::*;
use zenith_backend::soroban::jsonrpc::ScriptedJsonRpc;

const SETTLEMENT: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

async fn pool() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    ensure_schema(&pool).await.unwrap();
    pool
}

fn ctx(stage: Stage) -> AttemptContext {
    AttemptContext {
        attempt_id: "attempt-1".into(),
        tx_hash: Some("f00d".into()),
        stage,
        contract_id: Some(SETTLEMENT.into()),
        function: Some("settle_series".into()),
    }
}

/// Diagnostic events as RPC returns them with `xdrFormat: "json"`: the
/// settlement contract calls the oracle (which returns), logs, then fails
/// with contract error #3 and the host unwinds.
fn diagnostic_events() -> Value {
    let hash = "00".repeat(32);
    let oracle = "11".repeat(32);
    json!([
        {"in_successful_contract_call": false, "event": {"ext": "v0", "contract_id": null, "type_": "diagnostic",
          "body": {"v0": {"topics": [{"symbol": "fn_call"}, {"bytes": hash}, {"symbol": "settle_series"}],
                          "data": {"vec": [{"symbol": "XLM_C_0_12"}]}}}}},
        {"in_successful_contract_call": true, "event": {"ext": "v0", "contract_id": hash, "type_": "diagnostic",
          "body": {"v0": {"topics": [{"symbol": "fn_call"}, {"bytes": oracle}, {"symbol": "lastprice"}],
                          "data": {"symbol": "XLM"}}}}},
        {"in_successful_contract_call": true, "event": {"ext": "v0", "contract_id": oracle, "type_": "diagnostic",
          "body": {"v0": {"topics": [{"symbol": "fn_return"}, {"symbol": "lastprice"}],
                          "data": {"i128": "1182000"}}}}},
        {"in_successful_contract_call": false, "event": {"ext": "v0", "contract_id": hash, "type_": "diagnostic",
          "body": {"v0": {"topics": [{"symbol": "log"}],
                          "data": {"vec": [{"string": "series lookup"}, {"symbol": "XLM_C_0_12"}]}}}}},
        {"in_successful_contract_call": false, "event": {"ext": "v0", "contract_id": hash, "type_": "diagnostic",
          "body": {"v0": {"topics": [{"symbol": "error"}, {"error": {"contract": 3}}],
                          "data": {"string": "escalating error to panic"}}}}},
        {"in_successful_contract_call": false, "event": {"ext": "v0", "contract_id": null, "type_": "diagnostic",
          "body": {"v0": {"topics": [{"symbol": "error"}, {"error": {"wasm_vm": "invalid_action"}}],
                          "data": {"vec": [{"string": "contract call failed"}, {"symbol": "settle_series"}]}}}}}
    ])
}

fn failed_get_transaction() -> Value {
    json!({
        "status": "FAILED",
        "ledger": 51234,
        "resultXdr": "AAAAAAAAAGT/////AAAAAQAAAAAAAAAY/////AAAAAA=",
        "resultJson": {"fee_charged": 100, "result": {"tx_failed": [{"op_inner": {"invoke_host_function": "trapped"}}]}},
        "diagnosticEventsJson": diagnostic_events(),
        "diagnosticEventsXdr": ["AAAAAQ=="]
    })
}

#[tokio::test]
async fn decodes_trapped_settlement_from_get_transaction() {
    let record = analyze(&ctx(Stage::Confirm), &failed_get_transaction(), 1_000).unwrap();

    assert!(record.decoded);
    assert_eq!(record.tx_result.as_deref(), Some("tx_failed"));
    assert_eq!(record.op_result.as_deref(), Some("trapped"));
    // The contract error is the root cause, not the host's unwinding error.
    assert_eq!(
        record.host_error,
        Some(HostError {
            kind: "contract".into(),
            code: "3".into()
        })
    );
    assert_eq!(record.contract_error_code, Some(3));
    assert_eq!(
        record.contract_error_name.as_deref(),
        Some("SeriesNotFound")
    );
    assert_eq!(
        record.error_message.as_deref(),
        Some("escalating error to panic")
    );
    assert_eq!(record.logs, vec!["series lookup, XLM_C_0_12".to_string()]);

    assert_eq!(record.call_trace.len(), 2);
    assert_eq!(record.call_trace[1].function.as_deref(), Some("lastprice"));
    assert!(record.call_trace[1].returned);
    assert_eq!(
        record.call_trace[1].return_value,
        Some(json!({"i128": "1182000"}))
    );
    let trapped = record.trapped_frame.as_ref().unwrap();
    assert_eq!(trapped.function.as_deref(), Some("settle_series"));
    assert_eq!(trapped.contract_id.as_deref(), Some(SETTLEMENT));
    assert_eq!(record.diagnostic_events_xdr, vec!["AAAAAQ==".to_string()]);
    assert_eq!(
        record.summary,
        format!("confirm failed: settle_series on {SETTLEMENT} — Error(contract, 3) SeriesNotFound [trapped]")
    );
}

#[tokio::test]
async fn simulation_failure_with_only_error_text_and_xdr() {
    let resp = json!({
        "latestLedger": 900,
        "error": "HostError: Error(Budget, ExceededLimit)\n\nEvent log (newest first): ...",
        "events": ["AAAAAAAAAAAAAAAB"]
    });
    let r = analyze(&ctx(Stage::Simulate), &resp, 0).unwrap();
    assert!(!r.decoded, "only XDR was available");
    assert_eq!(
        r.host_error,
        Some(HostError {
            kind: "budget".into(),
            code: "exceeded_limit".into()
        })
    );
    assert_eq!(
        r.error_message.as_deref(),
        Some("HostError: Error(Budget, ExceededLimit)")
    );
    assert_eq!(r.diagnostic_events_xdr.len(), 1);
    assert_eq!(r.ledger, Some(900));
    // Falls back to the intended call when there is no trace.
    assert!(r.summary.contains("settle_series"));
}

#[tokio::test]
async fn capture_persists_against_attempt_and_api_serves_it() {
    let db = pool().await;
    let store = ForensicsStore::new(db.clone());
    let rpc = ScriptedJsonRpc::new();
    rpc.push("getTransaction", Ok(failed_get_transaction()));

    let rec = store
        .capture_confirmed(&rpc, &ctx(Stage::Submit), 42)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rec.attempt.stage, Stage::Confirm);
    assert_eq!(
        rpc.calls()[0].1,
        json!({"hash": "f00d", "xdrFormat": "json"})
    );
    assert_eq!(store.get(&rec.id).await.unwrap().unwrap(), rec);

    // A successful transaction is not recorded.
    let ok = ScriptedJsonRpc::new();
    ok.push("getTransaction", Ok(json!({"status": "SUCCESS"})));
    assert!(store
        .capture_confirmed(&ok, &ctx(Stage::Confirm), 43)
        .await
        .unwrap()
        .is_none());

    let app = router(db.clone(), Some("s3cret".into()));
    let req = |uri: &str, token: Option<&str>| {
        let mut b = Request::get(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    };
    let body = |resp: axum::response::Response| async move {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };

    assert_eq!(
        app.clone().oneshot(req("/", None)).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(req("/", Some("wrong")))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let list = app
        .clone()
        .oneshot(req("/?attempt_id=attempt-1", Some("s3cret")))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list = body(list).await;
    assert_eq!(list["records"].as_array().unwrap().len(), 1);
    assert_eq!(list["records"][0]["contract_error_code"], 3);

    let other = body(
        app.clone()
            .oneshot(req("/?tx_hash=nope", Some("s3cret")))
            .await
            .unwrap(),
    )
    .await;
    assert!(other["records"].as_array().unwrap().is_empty());

    let one = app
        .clone()
        .oneshot(req(&format!("/{}", rec.id), Some("s3cret")))
        .await
        .unwrap();
    assert_eq!(one.status(), StatusCode::OK);
    assert_eq!(
        body(one).await["trapped_frame"]["function"],
        "settle_series"
    );
    assert_eq!(
        app.clone()
            .oneshot(req("/missing", Some("s3cret")))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    let disabled = router(db, None);
    assert_eq!(
        disabled.oneshot(req("/", Some(""))).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}
