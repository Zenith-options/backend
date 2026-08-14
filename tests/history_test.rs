mod common;

use axum::http::StatusCode;
use common::TestApp;

async fn open_and_close(app: &TestApp, token: &str) {
    let (_, opened) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": 70000, "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(token),
        )
        .await;
    let id = opened["id"].as_str().unwrap();
    app.post_with(
        &format!("/api/v1/positions/{id}/close"),
        serde_json::Value::Null,
        Some(token),
    )
    .await;
}

#[tokio::test]
async fn history_trades_are_paginated_but_stats_cover_everything() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    for _ in 0..5 {
        open_and_close(&app, &token).await;
    }

    let (status, page1) = app
        .get_with("/api/v1/history?limit=2&offset=0", Some(&token))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["trades"].as_array().unwrap().len(), 2);
    // Stats reflect all 5 closed trades, not just the 2 returned on this page.
    assert_eq!(page1["stats"]["trade_count"].as_i64().unwrap(), 5);

    let (_, page2) = app
        .get_with("/api/v1/history?limit=2&offset=2", Some(&token))
        .await;
    assert_eq!(page2["trades"].as_array().unwrap().len(), 2);
    assert_eq!(page2["stats"]["trade_count"].as_i64().unwrap(), 5);

    let (_, page3) = app
        .get_with("/api/v1/history?limit=2&offset=4", Some(&token))
        .await;
    assert_eq!(page3["trades"].as_array().unwrap().len(), 1);
    assert_eq!(page3["stats"]["trade_count"].as_i64().unwrap(), 5);

    let mut seen = std::collections::HashSet::new();
    for page in [&page1, &page2, &page3] {
        for trade in page["trades"].as_array().unwrap() {
            seen.insert(trade["id"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(
        seen.len(),
        5,
        "all 5 trades should appear exactly once across the 3 pages"
    );
}

#[tokio::test]
async fn history_with_no_trades_has_zeroed_stats() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, history) = app.get_with("/api/v1/history", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["trades"].as_array().unwrap().len(), 0);
    assert_eq!(history["stats"]["trade_count"].as_i64().unwrap(), 0);
    assert_eq!(
        history["stats"]["total_realized_pnl"].as_f64().unwrap(),
        0.0
    );
}
