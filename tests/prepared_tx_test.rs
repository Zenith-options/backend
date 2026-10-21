mod common;

use axum::http::StatusCode;
use common::TestApp;
use data_encoding::BASE64;

// Helper: prepare a deposit and return (handle, unsigned_xdr, token).
async fn do_prepare(app: &TestApp, token: &str) -> (String, String) {
    // Use a valid C... strkey (32 zero bytes).
    let contract = zenith_backend::strkey::encode_contract_id(&[0u8; 32]);
    let (status, body) = app
        .post_with(
            "/api/v1/tx/prepare",
            serde_json::json!({
                "operation": "deposit",
                "contract_address": contract,
                "amount": 500.0
            }),
            Some(token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "prepare failed: {body}");
    let handle      = body["handle"].as_str().unwrap().to_string();
    let unsigned_xdr = body["unsigned_xdr"].as_str().unwrap().to_string();
    assert!(!handle.is_empty());
    assert!(!unsigned_xdr.is_empty());
    assert_eq!(body["operation"].as_str().unwrap(), "deposit");
    assert!(body["expiration_ledger"].as_u64().unwrap() > 0);
    assert!(!body["summary"].as_str().unwrap().is_empty());
    (handle, unsigned_xdr)
}

#[tokio::test]
async fn prepare_returns_handle_and_unsigned_xdr() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    do_prepare(&app, &token).await;
}

#[tokio::test]
async fn prepare_requires_auth() {
    let app     = TestApp::spawn().await;
    let contract = zenith_backend::strkey::encode_contract_id(&[0u8; 32]);
    let (status, body) = app
        .post(
            "/api/v1/tx/prepare",
            serde_json::json!({"operation":"deposit","contract_address":contract,"amount":100.0}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn submit_accepts_unmodified_envelope() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (handle, unsigned_xdr) = do_prepare(&app, &token).await;

    // Wallet "signs" without modification (test scenario).
    let (status, body) = app
        .post_with(
            "/api/v1/tx/submit",
            serde_json::json!({"handle": handle, "signed_xdr": unsigned_xdr}),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    let tx_hash = body["tx_hash"].as_str().unwrap();
    assert!(!tx_hash.is_empty());
    assert_eq!(body["status"].as_str().unwrap(), "PENDING");
}

#[tokio::test]
async fn submit_rejects_replayed_handle() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (handle, unsigned_xdr) = do_prepare(&app, &token).await;

    let (first, _) = app
        .post_with("/api/v1/tx/submit", serde_json::json!({"handle":&handle,"signed_xdr":&unsigned_xdr}), Some(&token))
        .await;
    assert_eq!(first, StatusCode::OK);

    let (second, body) = app
        .post_with("/api/v1/tx/submit", serde_json::json!({"handle":&handle,"signed_xdr":&unsigned_xdr}), Some(&token))
        .await;
    assert_eq!(second, StatusCode::CONFLICT, "replay should be 409: {body}");
}

#[tokio::test]
async fn submit_rejects_tampered_amount() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (handle, unsigned_xdr) = do_prepare(&app, &token).await;

    let raw = BASE64.decode(unsigned_xdr.as_bytes()).unwrap();
    let mut env: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    env["operations"][0]["args"][0]["value"] = serde_json::json!("9999999");
    let tampered = BASE64.encode(serde_json::to_string(&env).unwrap().as_bytes());

    let (status, body) = app
        .post_with("/api/v1/tx/submit", serde_json::json!({"handle":handle,"signed_xdr":tampered}), Some(&token))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "tamper should be 422: {body}");
    assert!(body["error"].as_str().unwrap().contains("hash mismatch"));
}

#[tokio::test]
async fn submit_rejects_wrong_signer() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (handle, unsigned_xdr) = do_prepare(&app, &token).await;

    let attacker = zenith_backend::strkey::encode_stellar_public_key(&[9u8; 32]);
    let raw = BASE64.decode(unsigned_xdr.as_bytes()).unwrap();
    let mut env: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    env["source_account"] = serde_json::json!(attacker);
    let spoofed = BASE64.encode(serde_json::to_string(&env).unwrap().as_bytes());

    let (status, body) = app
        .post_with("/api/v1/tx/submit", serde_json::json!({"handle":handle,"signed_xdr":spoofed}), Some(&token))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "wrong signer should be 422: {body}");
    assert!(body["error"].as_str().unwrap().contains("different address"));
}

#[tokio::test]
async fn submit_rejects_extra_operations() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (handle, unsigned_xdr) = do_prepare(&app, &token).await;

    let raw = BASE64.decode(unsigned_xdr.as_bytes()).unwrap();
    let mut env: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let extra = env["operations"][0].clone();
    env["operations"].as_array_mut().unwrap().push(extra);
    let stuffed = BASE64.encode(serde_json::to_string(&env).unwrap().as_bytes());

    let (status, body) = app
        .post_with("/api/v1/tx/submit", serde_json::json!({"handle":handle,"signed_xdr":stuffed}), Some(&token))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "extra ops should be 400: {body}");
}

#[tokio::test]
async fn submit_rejects_fee_bump_wrapper() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (handle, unsigned_xdr) = do_prepare(&app, &token).await;

    let raw = BASE64.decode(unsigned_xdr.as_bytes()).unwrap();
    let mut env: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    env["fee_bump"] = serde_json::json!({"base_fee":200});
    let bumped = BASE64.encode(serde_json::to_string(&env).unwrap().as_bytes());

    let (status, body) = app
        .post_with("/api/v1/tx/submit", serde_json::json!({"handle":handle,"signed_xdr":bumped}), Some(&token))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "fee-bump should be 400: {body}");
}

#[tokio::test]
async fn get_preparation_returns_status() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (handle, _) = do_prepare(&app, &token).await;

    let (status, body) = app
        .get_with(&format!("/api/v1/tx/{handle}"), Some(&token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["handle"].as_str().unwrap(), handle);
    assert_eq!(body["status"].as_str().unwrap(), "pending");
    assert_eq!(body["operation"].as_str().unwrap(), "deposit");
}

#[tokio::test]
async fn get_preparation_404s_for_wrong_wallet() {
    let app    = TestApp::spawn().await;
    let owner  = app.login().await;
    let stranger = app.login().await;
    let (handle, _) = do_prepare(&app, &owner).await;

    let (status, _) = app
        .get_with(&format!("/api/v1/tx/{handle}"), Some(&stranger))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn prepare_rejects_invalid_contract_address() {
    let app   = TestApp::spawn().await;
    let token = app.login().await;
    let (status, body) = app
        .post_with(
            "/api/v1/tx/prepare",
            serde_json::json!({"operation":"deposit","contract_address":"not-a-contract","amount":100.0}),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
