mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn watchlist_chain_events_are_delivered_once_per_subscriber() {
    let app = TestApp::spawn().await;
    let (wallet, token) = app.login_account().await;

    let (status, _) = app
        .post_with(
            "/api/v1/watchlist",
            serde_json::json!({
                "underlying": "BTC",
                "event_types": ["settlement_completed", "vault_share_price_move"]
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, watchlist) = app.get_with("/api/v1/watchlist", Some(&token)).await;
    assert_eq!(
        watchlist[0]["event_types"],
        serde_json::json!(["settlement_completed", "vault_share_price_move"])
    );

    sqlx::query(
        "INSERT INTO webhook_endpoints
            (id, wallet_address, url, secret, event_types, active)
         VALUES ('hook-1', ?, 'https://example.test/hook', 'secret',
                 '[\"settlement_completed\"]', 1)",
    )
    .bind(&wallet)
    .execute(&app.state.db)
    .await
    .unwrap();

    let event = serde_json::json!({ "amount": "4200000" });
    let first = zenith_backend::alerts::dispatch_chain_event(
        &app.state,
        "BTC",
        "settlement_completed",
        "ledger-500:tx-1:event-2",
        event.clone(),
    )
    .await
    .unwrap();
    let replay = zenith_backend::alerts::dispatch_chain_event(
        &app.state,
        "BTC",
        "settlement_completed",
        "ledger-500:tx-1:event-2",
        event,
    )
    .await
    .unwrap();

    assert_eq!(first, 1);
    assert_eq!(replay, 0);
    let deliveries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM delivery_events
         WHERE wallet_address = ? AND event_type = 'settlement_completed'",
    )
    .bind(wallet)
    .fetch_one(&app.state.db)
    .await
    .unwrap();
    assert_eq!(deliveries, 1);
}

#[tokio::test]
async fn watchlist_rejects_unsupported_chain_event_types() {
    let app = TestApp::spawn().await;
    let token = app.login().await;
    let (status, _) = app
        .post_with(
            "/api/v1/watchlist",
            serde_json::json!({
                "underlying": "BTC",
                "event_types": ["unknown_event"]
            }),
            Some(&token),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}
