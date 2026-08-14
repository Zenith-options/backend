mod common;

use axum::http::StatusCode;
use common::TestApp;

/// Every other error path in this API returns a JSON `{"error": "..."}`
/// body via AppError — axum's default Query<T>/Json<T> rejections are
/// plain text, which used to break that contract for any malformed query
/// param or request body. Covers both AppQuery and AppJson.

#[tokio::test]
async fn malformed_price_query_returns_json_not_plain_text() {
    let app = TestApp::spawn().await;
    let (status, body) = app
        .get("/api/v1/price?underlying=BTC&strike=not-a-number&expiry_days=30&option_type=call")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().is_some(),
        "expected a JSON {{\"error\": ...}} body, got: {body:?}"
    );
}

#[tokio::test]
async fn malformed_positions_limit_query_returns_json_not_plain_text() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, body) = app
        .get_with("/api/v1/positions?limit=not-a-number", Some(&token))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().is_some(),
        "expected a JSON {{\"error\": ...}} body, got: {body:?}"
    );
}

#[tokio::test]
async fn malformed_history_offset_query_returns_json_not_plain_text() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, body) = app
        .get_with("/api/v1/history?offset=not-a-number", Some(&token))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().is_some(),
        "expected a JSON {{\"error\": ...}} body, got: {body:?}"
    );
}

#[tokio::test]
async fn malformed_open_position_body_returns_json_not_plain_text() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, body) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": "not-a-number", "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"].as_str().is_some(),
        "expected a JSON {{\"error\": ...}} body, got: {body:?}"
    );
}

#[tokio::test]
async fn malformed_auth_nonce_body_returns_json_not_plain_text() {
    let app = TestApp::spawn().await;

    let (status, body) = app
        .post(
            "/api/v1/auth/nonce",
            serde_json::json!({ "wallet_address": 12345 }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"].as_str().is_some(),
        "expected a JSON {{\"error\": ...}} body, got: {body:?}"
    );
}
