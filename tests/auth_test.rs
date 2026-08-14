mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn full_login_round_trip_then_me() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, body) = app.get_with("/api/v1/auth/me", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["wallet_address"].as_str().unwrap().starts_with('G'));
}

#[tokio::test]
async fn me_without_token_is_unauthorized() {
    let app = TestApp::spawn().await;
    let (status, body) = app.get("/api/v1/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].as_str().is_some());
}

#[tokio::test]
async fn me_with_garbage_token_is_unauthorized() {
    let app = TestApp::spawn().await;
    let (status, _) = app.get_with("/api/v1/auth/me", Some("not-a-real-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn nonce_rejects_malformed_wallet_address() {
    let app = TestApp::spawn().await;
    let (status, _) = app
        .post("/api/v1/auth/nonce", serde_json::json!({ "wallet_address": "not-real" }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn verify_rejects_a_replayed_nonce() {
    use data_encoding::BASE64;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::RngCore;

    let app = TestApp::spawn().await;

    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let address = zenith_backend::strkey::encode_stellar_public_key(signing_key.verifying_key().as_bytes());

    let (_, nonce_resp) = app
        .post("/api/v1/auth/nonce", serde_json::json!({ "wallet_address": address }))
        .await;
    let message = nonce_resp["message"].as_str().unwrap().to_string();
    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = BASE64.encode(&signature.to_bytes());

    let verify_body = serde_json::json!({ "wallet_address": address, "message": message, "signature": sig_b64 });

    let (first_status, first_body) = app.post("/api/v1/auth/verify", verify_body.clone()).await;
    assert_eq!(first_status, StatusCode::OK);
    assert!(first_body["token"].as_str().is_some());

    let (second_status, _) = app.post("/api/v1/auth/verify", verify_body).await;
    assert_eq!(second_status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn two_logins_get_different_tokens() {
    let app = TestApp::spawn().await;
    let token1 = app.login().await;
    let token2 = app.login().await;
    assert_ne!(token1, token2);
}
