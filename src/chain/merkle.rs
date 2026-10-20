//! Deterministic Merkle commitment over a batch of settlement payouts
//! (issue #196).
//!
//! The tree commits to a set of payout instructions. Its construction is
//! fully specified here so that an independent implementation (a Soroban
//! contract verifier, an SDK, an auditor's script) can recompute the exact
//! same root and proofs from the same leaf set. Nothing in this module is
//! allowed to depend on hash-map iteration order, insertion order, wall
//! clock, or locale.
//!
//! ## Encoding (leaf v1)
//!
//! Each leaf is the SHA-256 of the concatenation below. `LEAF_DOMAIN` is a
//! fixed ASCII tag that domain-separates leaves from internal nodes so a leaf
//! hash can never be mistaken for an internal node and vice versa.
//!
//! ```text
//! leaf_hash = SHA-256(
//!     "ZENITH_SETTLE_LEAF_V1"      ||   // 21-byte domain tag
//!     u32_le(len(account))         ||   // 4 bytes
//!     account_bytes                ||   // UTF-8
//!     u32_le(len(position_id))     ||   // 4 bytes
//!     position_id_bytes            ||   // UTF-8
//!     u32_le(len(payout_asset))    ||   // 4 bytes
//!     payout_asset_bytes           ||   // UTF-8
//!     i64_le(amount_scaled)             // 8 bytes, amount * 10^7
//! )
//! ```
//!
//! `amount_scaled` is the payout amount in stroops (7 decimal places), as a
//! two's-complement little-endian `i64`. Variable-length strings are
//! length-prefixed to make the encoding injective (no ambiguity between
//! `"ab"+"c"` and `"a"+"bc"`).
//!
//! ## Encoding (internal node v1)
//!
//! ```text
//! node_hash = SHA-256("ZENITH_SETTLE_NODE_V1" || left || right)
//! ```
//!
//! with a 21-byte `NODE_DOMAIN` tag.
//!
//! ## Tree shape
//!
//! Leaves are first sorted by `(position_id, account)` — the canonical order,
//! independent of how the caller supplied them. The tree is then built
//! bottom-up: at each level, adjacent pairs are hashed; an odd trailing node
//! is *promoted unchanged* to the next level (it is never hashed with itself,
//! which would enable second-preimage-style ambiguity). The leaf count is part
//! of the on-chain commitment, so promotion cannot be confused with a
//! differently-shaped tree of the same root. The root of an empty tree is 32
//! zero bytes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Domain tag for leaf hashing (ASCII). Bump the trailing version when the
/// leaf encoding changes; old batches keep verifying with their own version.
pub const LEAF_DOMAIN: &[u8] = b"ZENITH_SETTLE_LEAF_V1";

/// Domain tag for internal-node hashing (ASCII).
pub const NODE_DOMAIN: &[u8] = b"ZENITH_SETTLE_NODE_V1";

/// The current leaf encoding version. Stamped onto persisted batches so a
/// future encoding change can be detected and handled explicitly.
pub const LEAF_VERSION: u32 = 1;

/// A 32-byte SHA-256 digest.
pub type Hash = [u8; 32];

/// A leaf in the settlement tree: one payout to one account.
///
/// `amount_scaled` is the payout in stroops (7 dp). Constructing a
/// [`SettlementLeaf`] from user-facing values is done with
/// [`SettlementLeaf::new`], which is the only place the scaling convention is
/// applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementLeaf {
    /// Stellar account (`G...`) or Soroban contract address (`C...`) that
    /// receives the payout.
    pub account: String,
    /// The position this payout settles. Unique within a batch.
    pub position_id: String,
    /// Asset the payout is denominated in (e.g. `"USDC"` or a contract id).
    pub payout_asset: String,
    /// Payout amount in stroops (amount * 10^7).
    pub amount_scaled: i64,
}

impl SettlementLeaf {
    /// Builds a leaf from a human-readable amount, scaling by 10^7 and
    /// rounding to the nearest stroop. Returns `None` if the scaled value
    /// does not fit in an `i64`.
    pub fn new(
        account: impl Into<String>,
        position_id: impl Into<String>,
        payout_asset: impl Into<String>,
        amount: f64,
    ) -> Option<Self> {
        if !amount.is_finite() {
            return None;
        }
        let scaled = (amount * 1e7).round();
        if scaled < i64::MIN as f64 || scaled > i64::MAX as f64 {
            return None;
        }
        Some(Self {
            account: account.into(),
            position_id: position_id.into(),
            payout_asset: payout_asset.into(),
            amount_scaled: scaled as i64,
        })
    }

    /// The amount as a float (payout in whole units).
    pub fn amount(&self) -> f64 {
        self.amount_scaled as f64 / 1e7
    }

    /// The canonical leaf hash (leaf v1). See the module docs for the exact
    /// byte layout.
    pub fn hash(&self) -> Hash {
        let mut h = Sha256::new();
        h.update(LEAF_DOMAIN);
        update_len_prefixed(&mut h, self.account.as_bytes());
        update_len_prefixed(&mut h, self.position_id.as_bytes());
        update_len_prefixed(&mut h, self.payout_asset.as_bytes());
        h.update(self.amount_scaled.to_le_bytes());
        h.finalize().into()
    }

    /// The canonical sort key: `(position_id, account)`. Both fields being
    /// unique-ish in practice, this is a total order over a well-formed leaf
    /// set.
    fn sort_key(&self) -> (&str, &str) {
        (&self.position_id, &self.account)
    }
}

fn update_len_prefixed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u32).to_le_bytes());
    h.update(bytes);
}

/// Hashes a parent from its two children (node v1).
fn hash_node(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update(NODE_DOMAIN);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// The empty-tree root.
pub fn empty_root() -> Hash {
    [0u8; 32]
}

/// A single step of a Merkle inclusion proof. `sibling` is the hash that is
/// combined with the running value; `sibling_is_right` says whether the
/// sibling sits to the right of the running value at this level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofStep {
    pub sibling: Hash,
    pub sibling_is_right: bool,
}

impl ProofStep {
    /// Applies this step to a running hash.
    pub fn apply(&self, running: &Hash) -> Hash {
        if self.sibling_is_right {
            hash_node(running, &self.sibling)
        } else {
            hash_node(&self.sibling, running)
        }
    }
}

/// A complete Merkle inclusion proof for one leaf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleProof {
    pub leaf_version: u32,
    pub leaf: Hash,
    pub steps: Vec<ProofStep>,
}

impl MerkleProof {
    /// Recomputes the root this proof claims to be against. Compare the
    /// result to the committed root to verify inclusion.
    pub fn root(&self) -> Hash {
        let mut running = self.leaf;
        for step in &self.steps {
            running = step.apply(&running);
        }
        running
    }

    /// Convenience: verifies against an expected root.
    pub fn verify(&self, expected_root: &Hash) -> bool {
        &self.root() == expected_root
    }
}

/// The full commitment for a batch: the root, the number of leaves it covers,
/// and the leaf encoding version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commitment {
    pub root: Hash,
    pub leaf_count: u64,
    pub leaf_version: u32,
    /// Sum of every leaf's `amount_scaled`. Included so the on-chain
    /// commitment can be checked against the batch's total payout.
    pub total_amount_scaled: i64,
}

/// A built tree. Holds the canonical leaf ordering so proofs can be extracted
/// for any leaf without re-sorting.
#[derive(Debug, Clone)]
pub struct MerkleTree {
    leaves: Vec<SettlementLeaf>,
    /// `levels[0]` is the leaf-hash level; the last level is a single root
    /// (or empty for an empty tree).
    levels: Vec<Vec<Hash>>,
}

impl MerkleTree {
    /// Builds a tree from an arbitrary collection of leaves. Leaves are
    /// sorted into canonical order; the caller's order does not matter.
    ///
    /// # Panics
    /// Panics if two leaves share the same `(position_id, account)` — the
    /// sort key must be unique for proofs to be unambiguous. Use
    /// [`MerkleTree::try_build`] for a fallible version.
    pub fn build(leaves: impl IntoIterator<Item = SettlementLeaf>) -> Self {
        Self::try_build(leaves).expect("duplicate (position_id, account) in leaf set")
    }

    /// Fallible constructor: rejects duplicate sort keys.
    pub fn try_build(
        leaves: impl IntoIterator<Item = SettlementLeaf>,
    ) -> Result<Self, MerkleError> {
        let mut leaves: Vec<SettlementLeaf> = leaves.into_iter().collect();
        leaves.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        for pair in leaves.windows(2) {
            if pair[0].sort_key() == pair[1].sort_key() {
                return Err(MerkleError::DuplicateLeaf {
                    position_id: pair[0].position_id.clone(),
                    account: pair[0].account.clone(),
                });
            }
        }
        let mut levels: Vec<Vec<Hash>> = Vec::new();
        if !leaves.is_empty() {
            levels.push(leaves.iter().map(SettlementLeaf::hash).collect());
            while levels.last().unwrap().len() > 1 {
                let current = levels.last().unwrap();
                let mut next = Vec::with_capacity(current.len().div_ceil(2));
                let mut i = 0;
                while i < current.len() {
                    if i + 1 < current.len() {
                        next.push(hash_node(&current[i], &current[i + 1]));
                    } else {
                        // Odd trailing node: promote unchanged, never
                        // hash-with-self.
                        next.push(current[i]);
                    }
                    i += 2;
                }
                levels.push(next);
            }
        }
        Ok(Self { leaves, levels })
    }

    /// The root hash (zero bytes for an empty tree).
    pub fn root(&self) -> Hash {
        self.levels
            .last()
            .and_then(|level| level.first().copied())
            .unwrap_or_else(empty_root)
    }

    /// The commitment (root + leaf count + version + total payout).
    pub fn commitment(&self) -> Commitment {
        Commitment {
            root: self.root(),
            leaf_count: self.leaves.len() as u64,
            leaf_version: LEAF_VERSION,
            total_amount_scaled: self
                .leaves
                .iter()
                .map(|l| l.amount_scaled)
                .fold(0i64, i64::saturating_add),
        }
    }

    /// Leaves in canonical order.
    pub fn leaves(&self) -> &[SettlementLeaf] {
        &self.leaves
    }

    /// Extracts an inclusion proof for the leaf at `index` (canonical order).
    pub fn proof(&self, index: usize) -> Option<MerkleProof> {
        if index >= self.leaves.len() {
            return None;
        }
        let mut steps = Vec::new();
        let mut idx = index;
        for level in &self.levels {
            if level.len() <= 1 {
                break;
            }
            if idx % 2 == 0 {
                // Left child: sibling to the right, if present.
                if idx + 1 < level.len() {
                    steps.push(ProofStep {
                        sibling: level[idx + 1],
                        sibling_is_right: true,
                    });
                }
                // else: promoted node has no sibling this level; skip.
            } else {
                steps.push(ProofStep {
                    sibling: level[idx - 1],
                    sibling_is_right: false,
                });
            }
            idx /= 2;
        }
        Some(MerkleProof {
            leaf_version: LEAF_VERSION,
            leaf: self.leaves[index].hash(),
            steps,
        })
    }

    /// Extracts a proof for a leaf by its `(position_id, account)`.
    pub fn proof_for(&self, position_id: &str, account: &str) -> Option<MerkleProof> {
        let index = self
            .leaves
            .iter()
            .position(|l| l.position_id == position_id && l.account == account)?;
        self.proof(index)
    }
}

/// Errors from tree construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MerkleError {
    DuplicateLeaf {
        position_id: String,
        account: String,
    },
}

impl std::fmt::Display for MerkleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MerkleError::DuplicateLeaf {
                position_id,
                account,
            } => write!(
                f,
                "duplicate leaf for position {position_id} / account {account}"
            ),
        }
    }
}

impl std::error::Error for MerkleError {}

/// Renders a hash as lowercase hex.
pub fn to_hex(hash: &Hash) -> String {
    data_encoding::HEXLOWER.encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(pos: &str, account: &str, amount: f64) -> SettlementLeaf {
        SettlementLeaf::new(account, pos, "USDC", amount).unwrap()
    }

    #[test]
    fn leaf_encoding_is_deterministic_and_domain_separated() {
        let a = leaf("p1", "GAA", 10.0);
        let b = leaf("p1", "GAA", 10.0);
        assert_eq!(a.hash(), b.hash());

        // Domain separation: a leaf hash must not equal a raw SHA-256 of the
        // same payload without the tag.
        let mut raw = Sha256::new();
        update_len_prefixed(&mut raw, b"GAA");
        update_len_prefixed(&mut raw, b"p1");
        update_len_prefixed(&mut raw, b"USDC");
        raw.update(100_000_000i64.to_le_bytes());
        let raw: Hash = raw.finalize().into();
        assert_ne!(a.hash(), raw);
    }

    #[test]
    fn leaf_encoding_is_injective_across_field_boundaries() {
        // ("ab","c") and ("a","bc") must not collide.
        let x = SettlementLeaf::new("ab", "c", "USDC", 1.0).unwrap();
        let y = SettlementLeaf::new("a", "bc", "USDC", 1.0).unwrap();
        assert_ne!(x.hash(), y.hash());
    }

    #[test]
    fn order_does_not_affect_root() {
        let leaves = vec![
            leaf("p3", "GCC", 3.0),
            leaf("p1", "GAA", 1.0),
            leaf("p2", "GBB", 2.0),
        ];
        let mut shuffled = leaves.clone();
        shuffled.reverse();
        assert_eq!(
            MerkleTree::build(leaves).root(),
            MerkleTree::build(shuffled).root()
        );
    }

    #[test]
    fn single_leaf_root_is_the_leaf_hash() {
        let l = leaf("p1", "GAA", 1.0);
        let tree = MerkleTree::build(vec![l.clone()]);
        assert_eq!(tree.root(), l.hash());
        assert_eq!(tree.commitment().leaf_count, 1);
    }

    #[test]
    fn empty_tree_root_is_zero() {
        let tree = MerkleTree::build(Vec::new());
        assert_eq!(tree.root(), empty_root());
        assert_eq!(tree.commitment().leaf_count, 0);
        assert_eq!(tree.commitment().total_amount_scaled, 0);
    }

    #[test]
    fn proofs_verify_for_every_leaf_including_odd_promotions() {
        for n in 1..=9usize {
            let leaves: Vec<_> = (0..n)
                .map(|i| leaf(&format!("p{i:02}"), &format!("G{i:02}"), i as f64 + 1.0))
                .collect();
            let tree = MerkleTree::build(leaves);
            let root = tree.root();
            for i in 0..n {
                let proof = tree.proof(i).unwrap();
                assert!(proof.verify(&root), "proof for leaf {i} of {n} failed");
            }
        }
    }

    #[test]
    fn proof_for_leaf_lookup_by_id_matches_index() {
        let leaves: Vec<_> = (0..5)
            .map(|i| leaf(&format!("p{i}"), &format!("G{i}"), i as f64 + 1.0))
            .collect();
        let tree = MerkleTree::build(leaves);
        let by_index = tree.proof(2).unwrap();
        let by_id = tree.proof_for("p2", "G2").unwrap();
        assert_eq!(by_index, by_id);
    }

    #[test]
    fn tampered_proof_fails_verification() {
        let leaves: Vec<_> = (0..4)
            .map(|i| leaf(&format!("p{i}"), &format!("G{i}"), i as f64 + 1.0))
            .collect();
        let tree = MerkleTree::build(leaves);
        let mut proof = tree.proof(0).unwrap();
        proof.leaf = leaf("p0", "G0", 999.0).hash();
        assert!(!proof.verify(&tree.root()));
    }

    #[test]
    fn duplicate_sort_keys_are_rejected() {
        let leaves = vec![leaf("p1", "GAA", 1.0), leaf("p1", "GAA", 2.0)];
        assert!(matches!(
            MerkleTree::try_build(leaves),
            Err(MerkleError::DuplicateLeaf { .. })
        ));
    }

    #[test]
    fn commitment_total_is_sum_of_amounts() {
        let leaves = vec![leaf("p1", "GAA", 1.5), leaf("p2", "GBB", 2.25)];
        let c = MerkleTree::build(leaves).commitment();
        assert_eq!(c.total_amount_scaled, 37_500_000);
        assert_eq!(c.leaf_version, LEAF_VERSION);
    }
}
