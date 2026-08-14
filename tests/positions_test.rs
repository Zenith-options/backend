mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn open_long_call_debits_premium_from_balance() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (_, account_before) = app.get_with("/api/v1/account", Some(&token)).await;
    let balance_before = account_before["balance"].as_f64().unwrap();

    let (status, position) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": 70000, "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let premium = position["entry_premium"].as_f64().unwrap();
    assert!(premium > 0.0);

    let (_, account_after) = app.get_with("/api/v1/account", Some(&token)).await;
    let balance_after = account_after["balance"].as_f64().unwrap();
    assert!((balance_before - premium - balance_after).abs() < 1e-6);
}

#[tokio::test]
async fn open_rejects_unknown_underlying() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, body) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "DOGE", "strike": 1, "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("DOGE"));
}

#[tokio::test]
async fn open_rejects_position_exceeding_buying_power() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, _) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": 70000, "expiry_days": 30,
                "option_type": "put", "position_type": "short", "contracts": 1000
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn close_settles_position_and_appears_in_history() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (_, opened) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": 70000, "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(&token),
        )
        .await;
    let id = opened["id"].as_str().unwrap();

    let (status, closed) = app
        .post_with(
            &format!("/api/v1/positions/{id}/close"),
            serde_json::Value::Null,
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(closed["status"].as_str().unwrap(), "closed");
    // Spot hasn't moved between open and close in this test, so pricing
    // with the same inputs should round-trip to zero pnl exactly.
    assert_eq!(closed["realized_pnl"].as_f64().unwrap(), 0.0);

    let (status, history) = app.get_with("/api/v1/history", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["stats"]["trade_count"].as_i64().unwrap(), 1);
    assert_eq!(history["trades"][0]["id"].as_str().unwrap(), id);
}

#[tokio::test]
async fn closing_twice_404s_the_second_time() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (_, opened) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": 70000, "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(&token),
        )
        .await;
    let id = opened["id"].as_str().unwrap();

    let (first, _) = app
        .post_with(
            &format!("/api/v1/positions/{id}/close"),
            serde_json::Value::Null,
            Some(&token),
        )
        .await;
    assert_eq!(first, StatusCode::OK);

    let (second, _) = app
        .post_with(
            &format!("/api/v1/positions/{id}/close"),
            serde_json::Value::Null,
            Some(&token),
        )
        .await;
    assert_eq!(second, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn roll_relabels_old_leg_as_rolled_and_opens_replacement() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (_, opened) = app
        .post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "XLM", "strike": 0.13, "expiry_days": 30,
                "option_type": "call", "position_type": "short", "contracts": 100
            }),
            Some(&token),
        )
        .await;
    let id = opened["id"].as_str().unwrap();

    let (status, roll_result) = app
        .post_with(
            &format!("/api/v1/positions/{id}/roll"),
            serde_json::json!({ "new_strike": 0.14, "new_expiry_days": 60 }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(roll_result["closed"]["status"].as_str().unwrap(), "rolled");
    assert_eq!(roll_result["opened"]["status"].as_str().unwrap(), "open");
    assert_eq!(roll_result["opened"]["strike"].as_f64().unwrap(), 0.14);

    let (_, all_positions) = app.get_with("/api/v1/positions", Some(&token)).await;
    assert_eq!(all_positions.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn strategy_execute_shares_one_strategy_id_across_legs() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, legs) = app
        .post_with(
            "/api/v1/strategies/execute",
            serde_json::json!({
                "legs": [
                    { "underlying": "BTC", "strike": 68000, "expiry_days": 30, "option_type": "call", "position_type": "long", "contracts": 1 },
                    { "underlying": "BTC", "strike": 72000, "expiry_days": 30, "option_type": "call", "position_type": "short", "contracts": 1 }
                ]
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let legs = legs.as_array().unwrap();
    assert_eq!(legs.len(), 2);
    let sid0 = legs[0]["strategy_id"].as_str().unwrap();
    let sid1 = legs[1]["strategy_id"].as_str().unwrap();
    assert_eq!(sid0, sid1);
}

#[tokio::test]
async fn strategy_execute_is_atomic_on_partial_failure() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, _) = app
        .post_with(
            "/api/v1/strategies/execute",
            serde_json::json!({
                "legs": [
                    { "underlying": "BTC", "strike": 68000, "expiry_days": 30, "option_type": "call", "position_type": "long", "contracts": 1 },
                    { "underlying": "DOGE", "strike": 1, "expiry_days": 30, "option_type": "call", "position_type": "long", "contracts": 1 }
                ]
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, positions) = app.get_with("/api/v1/positions", Some(&token)).await;
    assert_eq!(
        positions.as_array().unwrap().len(),
        0,
        "the successful first leg must have been rolled back"
    );
}

#[tokio::test]
async fn list_positions_paginates_with_non_overlapping_pages() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    for _ in 0..5 {
        app.post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": 70000, "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(&token),
        )
        .await;
    }

    let (_, page1) = app
        .get_with("/api/v1/positions?limit=2&offset=0", Some(&token))
        .await;
    let (_, page2) = app
        .get_with("/api/v1/positions?limit=2&offset=2", Some(&token))
        .await;
    let (_, page3) = app
        .get_with("/api/v1/positions?limit=2&offset=4", Some(&token))
        .await;

    assert_eq!(page1.as_array().unwrap().len(), 2);
    assert_eq!(page2.as_array().unwrap().len(), 2);
    assert_eq!(
        page3.as_array().unwrap().len(),
        1,
        "only 1 position left for the last page"
    );

    let ids = |page: &serde_json::Value| -> std::collections::HashSet<String> {
        page.as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect()
    };
    let (ids1, ids2, ids3) = (ids(&page1), ids(&page2), ids(&page3));
    assert!(ids1.is_disjoint(&ids2));
    assert!(ids2.is_disjoint(&ids3));
    assert!(ids1.is_disjoint(&ids3));
}

#[tokio::test]
async fn list_positions_caps_an_excessive_limit() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    let (status, list) = app
        .get_with("/api/v1/positions?limit=999999", Some(&token))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_positions_reports_total_count_and_has_more_via_headers() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    for _ in 0..3 {
        app.post_with(
            "/api/v1/positions/open",
            serde_json::json!({
                "underlying": "BTC", "strike": 70000, "expiry_days": 30,
                "option_type": "call", "position_type": "long", "contracts": 1
            }),
            Some(&token),
        )
        .await;
    }

    let (status, headers, _) = app
        .get_raw("/api/v1/positions?limit=2", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("x-total-count").unwrap(), "3");
    assert_eq!(headers.get("x-has-more").unwrap(), "true");

    let (_, headers_last_page, _) = app
        .get_raw("/api/v1/positions?limit=2&offset=2", Some(&token), None)
        .await;
    assert_eq!(headers_last_page.get("x-total-count").unwrap(), "3");
    assert_eq!(headers_last_page.get("x-has-more").unwrap(), "false");
}
