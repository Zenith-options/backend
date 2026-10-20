//! End-to-end tests for Merkle-commitment batch settlement (issue #196).
//!
//! These drive the real router: plan a batch, commit it with a single signed
//! transaction reference, fetch an inclusion proof, and claim (fully and
//! partially). They also assert the two safety properties the issue calls
//! out — value conservation and leaf-set immutability after commitment.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use zenith_backend::chain::merkle::{to_hex, MerkleTree, SettlementLeaf};

/// Plans a two-leaf batch owned by `address`, commits it, and returns
/// `(batch_id, token, address, total)`.
async fn commit_two_leaf_batch(app: &TestApp) -> (String, String, String) {
    let (address, token) = app.login_account().await;
    let total = 300_000_000i64; // 30.0 USDC in stroops
    let body = json!({
        "payout_asset": "USDC",
        "total_scaled": total,
        "instructions": [
            { "position_id": "p1", "account": address, "amount_scaled": 100_000_000 },
            { "position_id": "p2", "account": address, "amount_scaled": 200_000_000 }
        ]
    });
    let (status, resp) = app
        .post_with("/api/v1/settlement/batches", body, Some(&token))
        .await;
    assert_eq!(status, StatusCode::CREATED, "plan failed: {resp}");
    let batch_id = resp["batch_id"].as_str().unwrap().to_string();

    let (status, resp) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/commit"),
            json!({ "commitment_tx_hash": "TXHASH_SIGNED_1", "commitment_ledger": 42 }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {resp}");
    assert_eq!(resp["status"], "committed");
    assert_eq!(resp["leaf_count"], 2);
    (batch_id, token, address)
}

#[tokio::test]
async fn plan_commit_and_fetch_proof_round_trips() {
    let app = TestApp::spawn().await;
    let (batch_id, token, _address) = commit_two_leaf_batch(&app).await;

    let (status, resp) = app
        .get_with(
            &format!("/api/v1/settlement/batches/{batch_id}"),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let root_hex = resp["root"].as_str().unwrap().to_string();

    let (status, proof) = app
        .get_with(
            &format!("/api/v1/settlement/batches/{batch_id}/leaves/p1/proof"),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(proof["root"].as_str().unwrap(), root_hex);
    assert_eq!(proof["leaf_version"], 1);

    // Reconstruct the tree independently and check the returned proof is
    // valid against the committed root.
    let account = proof["account"].as_str().unwrap().to_string();
    let leaves = vec![
        SettlementLeaf::new(account.clone(), "p1", "USDC", 10.0).unwrap(),
        SettlementLeaf::new(account, "p2", "USDC", 20.0).unwrap(),
    ];
    let tree = MerkleTree::build(leaves);
    assert_eq!(to_hex(&tree.root()), root_hex);

    // The independently computed proof must have exactly the steps the API
    // returned, in the same order, with the same sibling sides.
    let expected = tree
        .proof_for("p1", proof["account"].as_str().unwrap())
        .unwrap();
    let returned: Vec<(String, bool)> = proof["proof"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["sibling"].as_str().unwrap().to_string(),
                s["sibling_is_right"].as_bool().unwrap(),
            )
        })
        .collect();
    let expected: Vec<(String, bool)> = expected
        .steps
        .iter()
        .map(|s| (to_hex(&s.sibling), s.sibling_is_right))
        .collect();
    assert_eq!(returned, expected);
    assert_eq!(
        to_hex(&expected_leaf_hash(
            &tree,
            "p1",
            proof["account"].as_str().unwrap()
        )),
        proof["leaf"].as_str().unwrap()
    );
}

fn expected_leaf_hash(tree: &MerkleTree, position_id: &str, account: &str) -> [u8; 32] {
    tree.leaves()
        .iter()
        .find(|l| l.position_id == position_id && l.account == account)
        .unwrap()
        .hash()
}

#[tokio::test]
async fn conservation_violation_is_rejected_at_plan() {
    let app = TestApp::spawn().await;
    let (address, token) = app.login_account().await;
    // Declared total does not equal the leaves' sum.
    let (status, resp) = app
        .post_with(
            "/api/v1/settlement/batches",
            json!({
                "payout_asset": "USDC",
                "total_scaled": 999,
                "instructions": [
                    { "position_id": "p1", "account": address, "amount_scaled": 100_000_000 }
                ]
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(resp["code"], "invalid_batch");
}

#[tokio::test]
async fn duplicate_instruction_is_rejected() {
    let app = TestApp::spawn().await;
    let (address, token) = app.login_account().await;
    let (status, resp) = app
        .post_with(
            "/api/v1/settlement/batches",
            json!({
                "payout_asset": "USDC",
                "total_scaled": 200_000_000,
                "instructions": [
                    { "position_id": "p1", "account": address, "amount_scaled": 100_000_000 },
                    { "position_id": "p1", "account": address, "amount_scaled": 100_000_000 }
                ]
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(resp["code"], "invalid_batch");
}

#[tokio::test]
async fn partial_claims_accumulate_and_finalize() {
    let app = TestApp::spawn().await;
    let (batch_id, token, _address) = commit_two_leaf_batch(&app).await;

    // First partial claim on p1: 4.0 of 10.0.
    let (status, resp) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/claims"),
            json!({ "position_id": "p1", "amount_scaled": 40_000_000, "idempotency_key": "k1" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["idempotent_replay"], false);
    assert_eq!(resp["claimed_total_scaled"], 40_000_000);
    assert_eq!(resp["outstanding_scaled"], 60_000_000);

    // Replaying the same idempotency key must not double-claim.
    let (status, resp) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/claims"),
            json!({ "position_id": "p1", "amount_scaled": 40_000_000, "idempotency_key": "k1" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["idempotent_replay"], true);
    assert_eq!(resp["claimed_total_scaled"], 40_000_000);

    // Over-claiming the remainder must fail.
    let (status, resp) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/claims"),
            json!({ "position_id": "p1", "amount_scaled": 70_000_000, "idempotency_key": "k2" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(resp["code"], "over_claim");

    // Claim the rest of p1, then all of p2 — the batch should finalize.
    let (status, _) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/claims"),
            json!({ "position_id": "p1", "amount_scaled": 60_000_000, "idempotency_key": "k3" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/claims"),
            json!({ "position_id": "p2", "amount_scaled": 200_000_000, "idempotency_key": "k4" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, resp) = app
        .get_with(
            &format!("/api/v1/settlement/batches/{batch_id}"),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["status"], "finalized");
}

#[tokio::test]
async fn committed_batch_rejects_a_different_commitment() {
    let app = TestApp::spawn().await;
    let (batch_id, token, _address) = commit_two_leaf_batch(&app).await;

    // Re-committing the same hash is idempotent...
    let (status, _) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/commit"),
            json!({ "commitment_tx_hash": "TXHASH_SIGNED_1" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // ...but a different commitment is rejected: the leaf set is locked.
    let (status, resp) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/commit"),
            json!({ "commitment_tx_hash": "TXHASH_SIGNED_2" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(resp["code"], "leaf_set_locked");
}

#[tokio::test]
async fn claims_before_commitment_are_rejected() {
    let app = TestApp::spawn().await;
    let (address, token) = app.login_account().await;
    let (status, resp) = app
        .post_with(
            "/api/v1/settlement/batches",
            json!({
                "payout_asset": "USDC",
                "total_scaled": 100_000_000,
                "instructions": [
                    { "position_id": "p1", "account": address, "amount_scaled": 100_000_000 }
                ]
            }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = resp["batch_id"].as_str().unwrap();

    let (status, resp) = app
        .post_with(
            &format!("/api/v1/settlement/batches/{batch_id}/claims"),
            json!({ "position_id": "p1", "amount_scaled": 1_000_000, "idempotency_key": "k1" }),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(resp["code"], "invalid_state");
}
