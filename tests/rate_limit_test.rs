mod common;

use common::TestApp;

#[tokio::test]
async fn mutation_endpoints_are_rate_limited_per_ip() {
    let app = TestApp::spawn().await;
    let token = app.login().await;

    // add_watchlist is idempotent (ON CONFLICT DO NOTHING), so repeating it
    // rapidly can't fail on its own merits — any non-2xx status here can
    // only come from the rate limiter itself, not application logic.
    let mut statuses = Vec::new();
    for _ in 0..30 {
        let (status, _) = app
            .post_with(
                "/api/v1/watchlist",
                serde_json::json!({ "underlying": "BTC" }),
                Some(&token),
            )
            .await;
        statuses.push(status.as_u16());
    }

    assert!(
        statuses.contains(&429),
        "expected at least one 429 among 30 rapid requests against a burst_size=20 limit, got: {statuses:?}"
    );
}

#[tokio::test]
async fn auth_nonce_is_rate_limited_per_ip() {
    let app = TestApp::spawn().await;

    let mut statuses = Vec::new();
    for i in 0..20 {
        let (status, _) = app
            .post(
                "/api/v1/auth/nonce",
                serde_json::json!({ "wallet_address": format!("not-a-real-address-{i}") }),
            )
            .await;
        statuses.push(status.as_u16());
    }

    assert!(
        statuses.contains(&429),
        "expected at least one 429 among 20 rapid requests against a burst_size=10 limit, got: {statuses:?}"
    );
}
