//! Property tests for the Merkle settlement core (issue #196).
//!
//! These exercise the pure, deterministic logic — no database, no HTTP. They
//! are the "independent verifier" for the encoding: an implementation that
//! disagrees with any of these properties would produce roots/proofs that a
//! second implementation could not reproduce.

use proptest::prelude::*;
use zenith_backend::chain::batch::{
    apply_claim, check_conservation, plan_batch, SettlementInstruction,
};
use zenith_backend::chain::merkle::{MerkleTree, SettlementLeaf};

/// Distinct leaves: build from unique position ids so the sort key is unique.
fn arb_unique_leaves() -> impl Strategy<Value = Vec<SettlementLeaf>> {
    proptest::collection::btree_set("[a-z0-9]{1,8}", 0..16).prop_flat_map(|ids| {
        let ids: Vec<String> = ids.into_iter().collect();
        let n = ids.len();
        proptest::collection::vec(("[A-Z]{3}", 0i64..1_000_000_000i64), n).prop_map(move |attrs| {
            ids.iter()
                .zip(attrs)
                .map(|(position_id, (account, amount_scaled))| SettlementLeaf {
                    account,
                    position_id: position_id.clone(),
                    payout_asset: "USDC".to_string(),
                    amount_scaled,
                })
                .collect()
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// The root depends only on the *set* of leaves, not their order.
    #[test]
    fn root_is_order_independent(leaves in arb_unique_leaves(), seed in any::<u64>()) {
        let mut shuffled = leaves.clone();
        // Deterministic pseudo-shuffle seeded from `seed`.
        let n = shuffled.len();
        if n > 0 {
            let mut state = seed | 1;
            for i in (1..n).rev() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let j = (state >> 33) as usize % (i + 1);
                shuffled.swap(i, j);
            }
        }
        prop_assert_eq!(MerkleTree::build(leaves).root(), MerkleTree::build(shuffled).root());
    }

    /// Every leaf has an inclusion proof that verifies against the root.
    #[test]
    fn every_leaf_has_a_valid_proof(leaves in arb_unique_leaves()) {
        let tree = MerkleTree::build(leaves.clone());
        let root = tree.root();
        for leaf in tree.leaves() {
            let proof = tree
                .proof_for(&leaf.position_id, &leaf.account)
                .expect("proof for present leaf");
            prop_assert!(proof.verify(&root));
        }
    }

    /// Conservation holds exactly when the declared total equals the leaves'
    /// sum and is covered by available collateral.
    #[test]
    fn conservation_matches_arithmetic(
        amounts in proptest::collection::vec(0i64..1_000_000_000i64, 0..16),
        declared_adjust in -2i64..=2,
    ) {
        let leaves: Vec<SettlementInstruction> = amounts
            .iter()
            .enumerate()
            .map(|(i, a)| SettlementInstruction {
                position_id: format!("p{i}"),
                account: "GTEST".to_string(),
                payout_asset: "USDC".to_string(),
                amount_scaled: *a,
            })
            .collect();
        let sum: i64 = amounts.iter().sum();
        let declared = sum + declared_adjust;
        let available = sum.max(0);

        let expected_ok = declared == sum && declared <= available;
        let actual = check_conservation(&leaves, declared, available);
        prop_assert_eq!(actual.is_ok(), expected_ok);
    }

    /// A planned batch always carries a total equal to the sum of its leaves.
    #[test]
    fn planned_total_equals_leaf_sum(amounts in proptest::collection::vec(1i64..1_000_000i64, 1..16)) {
        let leaves: Vec<SettlementInstruction> = amounts
            .iter()
            .enumerate()
            .map(|(i, a)| SettlementInstruction {
                position_id: format!("p{i}"),
                account: "GTEST".to_string(),
                payout_asset: "USDC".to_string(),
                amount_scaled: *a,
            })
            .collect();
        let sum: i64 = amounts.iter().sum();
        let batch = plan_batch("USDC", leaves, sum, sum).unwrap();
        prop_assert_eq!(batch.commitment.total_amount_scaled, sum);
        prop_assert_eq!(batch.commitment.leaf_count, amounts.len() as u64);
    }

    /// Partial claims can never push a leaf past its committed amount.
    #[test]
    fn claims_never_exceed_committed(committed in 1i64..1_000_000_000i64, claim in 0i64..2_000_000_000i64) {
        let leaf = SettlementInstruction {
            position_id: "p".to_string(),
            account: "G".to_string(),
            payout_asset: "USDC".to_string(),
            amount_scaled: committed,
        };
        match apply_claim(&leaf, 0, claim) {
            Ok(total) => prop_assert!(total <= committed && claim > 0),
            Err(_) => prop_assert!(claim <= 0 || claim > committed),
        }
    }
}
