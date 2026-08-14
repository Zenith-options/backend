mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn add_list_delete_round_trip() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, _) = app.post_with("/api/v1/watchlist", serde_json::json!({ "underlying": "BTC" }), Some(&token)).await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, list) = app.get_with("/api/v1/watchlist", Some(&token)).await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["underlying"].as_str().unwrap(), "BTC");

    let (status, _) = app.delete_with("/api/v1/watchlist/BTC", &token).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list_after) = app.get_with("/api/v1/watchlist", Some(&token)).await;
    assert_eq!(list_after.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn adding_the_same_symbol_twice_is_idempotent() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (s1, _) = app.post_with("/api/v1/watchlist", serde_json::json!({ "underlying": "XLM" }), Some(&token)).await;
    let (s2, _) = app.post_with("/api/v1/watchlist", serde_json::json!({ "underlying": "XLM" }), Some(&token)).await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::CREATED);

    let (_, list) = app.get_with("/api/v1/watchlist", Some(&token)).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn adding_unknown_symbol_404s() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, _) = app.post_with("/api/v1/watchlist", serde_json::json!({ "underlying": "DOGE" }), Some(&token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_a_symbol_not_on_the_list_404s() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, _) = app.delete_with("/api/v1/watchlist/BTC", &token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn watchlist_is_scoped_per_wallet() {
    let app = TestApp::spawn().await;
    let token_a = app.login().await;
    let token_b = app.login().await;

    app.post_with("/api/v1/watchlist", serde_json::json!({ "underlying": "BTC" }), Some(&token_a)).await;

    let (_, list_b) = app.get_with("/api/v1/watchlist", Some(&token_b)).await;
    assert_eq!(list_b.as_array().unwrap().len(), 0, "wallet B must not see wallet A's watchlist");
}
