//! Merkle-commitment batch settlement for expired option pools (issue #196).
//!
//! When a pool of positions expires, the backend computes each position's
//! payout, commits to the whole payout set with a single Merkle root, submits
//! *one* signed transaction carrying that root (plus the leaf count and total
//! payout), and only then lets accounts claim their individual payouts by
//! proving inclusion.
//!
//! ## Lifecycle
//!
//! ```text
//!  PLANNED ──commit──▶ COMMITTED ──finalize──▶ FINALIZED
//! ```
//!
//! * **PLANNED** — the instruction set has been computed and persisted, but
//!   no root is on chain. Leaves may still be recomputed (the expiry may not
//!   be final yet) but the set is already frozen against edits that would
//!   break conservation.
//! * **COMMITTED** — a single signed transaction carrying the root, leaf
//!   count and total payout has been accepted. The leaf set is now
//!   **immutable**: adding, removing or editing a leaf after this point is
//!   rejected. Accounts may claim (fully or partially) against it.
//! * **FINALIZED** — every leaf has been fully claimed. No further claims are
//!   accepted.
//!
//! ## Conservation
//!
//! A batch is only committable if the sum of its leaves equals the batch's
//! declared total and the total does not exceed the pool's available
//! collateral ([`check_conservation`]). This is the "no value created" check
//! expressed purely in terms of the batch's own numbers, so it holds even if
//! the caller's inputs are wrong.
//!
//! ## Idempotency and partial claims
//!
//! Claims are keyed by `(batch_id, position_id, idempotency_key)`. Replaying
//! the same key returns the original result without moving money again.
//! A leaf may be claimed in several partial amounts as long as their sum never
//! exceeds the leaf's committed amount.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::merkle::{to_hex, Commitment, Hash, MerkleError, MerkleTree, SettlementLeaf};

/// A single settlement instruction: what a position is owed on expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementInstruction {
    pub position_id: String,
    pub account: String,
    pub payout_asset: String,
    /// Payout amount in stroops (7 dp).
    pub amount_scaled: i64,
}

impl SettlementInstruction {
    pub fn amount(&self) -> f64 {
        self.amount_scaled as f64 / 1e7
    }

    fn to_leaf(&self) -> SettlementLeaf {
        SettlementLeaf {
            account: self.account.clone(),
            position_id: self.position_id.clone(),
            payout_asset: self.payout_asset.clone(),
            amount_scaled: self.amount_scaled,
        }
    }
}

/// Batch lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchStatus {
    Planned,
    Committed,
    Finalized,
}

impl BatchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BatchStatus::Planned => "planned",
            BatchStatus::Committed => "committed",
            BatchStatus::Finalized => "finalized",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "planned" => Some(BatchStatus::Planned),
            "committed" => Some(BatchStatus::Committed),
            "finalized" => Some(BatchStatus::Finalized),
            _ => None,
        }
    }
}

/// Errors surfaced by batch operations. Handlers map these onto HTTP statuses
/// and stable error codes; keeping them as a typed enum lets the DB layer stay
/// independent of axum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchError {
    /// Two instructions share a `(position_id, account)`.
    DuplicateInstruction { position_id: String },
    /// The declared total does not equal the sum of leaves, or exceeds the
    /// pool's available collateral.
    ConservationViolated {
        declared: i64,
        actual: i64,
        available: i64,
    },
    /// The requested leaf set differs from the committed one.
    LeafSetLocked,
    /// A claim would push the leaf past its committed amount.
    OverClaim {
        position_id: String,
        committed: i64,
        would_be: i64,
    },
    /// No such batch.
    BatchNotFound { batch_id: String },
    /// No such leaf in the batch.
    LeafNotFound { position_id: String },
    /// The batch is not in a state that permits the operation.
    InvalidState {
        expected: &'static str,
        actual: String,
    },
    /// A leaf already fully claimed.
    NothingToClaim { position_id: String },
    /// Underlying Merkle construction error.
    Merkle(MerkleError),
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchError::DuplicateInstruction { position_id } => {
                write!(f, "duplicate settlement instruction for position {position_id}")
            }
            BatchError::ConservationViolated {
                declared,
                actual,
                available,
            } => write!(
                f,
                "conservation violated: declared {declared} != actual {actual}, available {available}"
            ),
            BatchError::LeafSetLocked => write!(f, "leaf set is locked after commitment"),
            BatchError::OverClaim {
                position_id,
                committed,
                would_be,
            } => write!(
                f,
                "claim for {position_id} would total {would_be}, exceeding committed {committed}"
            ),
            BatchError::BatchNotFound { batch_id } => write!(f, "no such batch {batch_id}"),
            BatchError::LeafNotFound { position_id } => {
                write!(f, "no leaf for position {position_id}")
            }
            BatchError::InvalidState { expected, actual } => {
                write!(f, "batch must be {expected}, is {actual}")
            }
            BatchError::NothingToClaim { position_id } => {
                write!(f, "position {position_id} has nothing left to claim")
            }
            BatchError::Merkle(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BatchError {}

impl From<MerkleError> for BatchError {
    fn from(e: MerkleError) -> Self {
        BatchError::Merkle(e)
    }
}

/// A value-conservation check over a batch's own numbers.
///
/// `declared_total` is what the batch promises to pay; `available` is the
/// collateral the pool actually holds. Both are in stroops. The batch is
/// only committable when the leaves sum exactly to the declared total and the
/// total is covered by available collateral.
pub fn check_conservation(
    leaves: &[SettlementInstruction],
    declared_total: i64,
    available: i64,
) -> Result<(), BatchError> {
    let actual: i64 = leaves
        .iter()
        .try_fold(0i64, |acc, l| acc.checked_add(l.amount_scaled))
        .ok_or(BatchError::ConservationViolated {
            declared: declared_total,
            actual: i64::MAX,
            available,
        })?;
    if actual != declared_total || declared_total > available {
        return Err(BatchError::ConservationViolated {
            declared: declared_total,
            actual,
            available,
        });
    }
    Ok(())
}

/// Builds a canonical tree from a set of instructions, rejecting duplicates.
pub fn build_tree(instructions: &[SettlementInstruction]) -> Result<MerkleTree, BatchError> {
    let mut seen = HashSet::new();
    for instr in instructions {
        if !seen.insert((instr.position_id.as_str(), instr.account.as_str())) {
            return Err(BatchError::DuplicateInstruction {
                position_id: instr.position_id.clone(),
            });
        }
    }
    let leaves: Vec<SettlementLeaf> = instructions.iter().map(|i| i.to_leaf()).collect();
    Ok(MerkleTree::try_build(leaves)?)
}

/// A planned batch: instructions plus the derived commitment, not yet on
/// chain. Created by [`plan_batch`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedBatch {
    pub payout_asset: String,
    pub instructions: Vec<SettlementInstruction>,
    pub commitment: Commitment,
    /// Collateral the batch was checked against (stroops).
    pub available_scaled: i64,
}

/// Plans a batch: applies conservation, builds the tree, and returns the
/// commitment. This is pure and does not touch the database, so it can be
/// unit-tested and reused by the on-chain submit path.
pub fn plan_batch(
    payout_asset: impl Into<String>,
    instructions: Vec<SettlementInstruction>,
    declared_total: i64,
    available_collateral: i64,
) -> Result<PlannedBatch, BatchError> {
    let payout_asset = payout_asset.into();
    if let Some(bad) = instructions.iter().find(|i| i.payout_asset != payout_asset) {
        return Err(BatchError::InvalidState {
            expected: "homogeneous payout_asset",
            actual: bad.payout_asset.clone(),
        });
    }
    check_conservation(&instructions, declared_total, available_collateral)?;
    let tree = build_tree(&instructions)?;
    Ok(PlannedBatch {
        payout_asset,
        instructions,
        commitment: tree.commitment(),
        available_scaled: available_collateral,
    })
}

/// A per-account view of a committed batch leaf.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeafView {
    pub position_id: String,
    pub account: String,
    pub amount_scaled: i64,
    pub claimed_scaled: i64,
    pub leaf_hash: String,
}

impl LeafView {
    pub fn outstanding_scaled(&self) -> i64 {
        self.amount_scaled.saturating_sub(self.claimed_scaled)
    }
}

/// Applies a (possibly partial) claim against a leaf, enforcing that the
/// running total never exceeds the committed amount. Returns the new claimed
/// total. Pure, so it is exhaustively testable.
pub fn apply_claim(
    leaf: &SettlementInstruction,
    already_claimed: i64,
    new_claim: i64,
) -> Result<i64, BatchError> {
    if new_claim <= 0 {
        return Err(BatchError::NothingToClaim {
            position_id: leaf.position_id.clone(),
        });
    }
    let would_be = already_claimed
        .checked_add(new_claim)
        .ok_or(BatchError::OverClaim {
            position_id: leaf.position_id.clone(),
            committed: leaf.amount_scaled,
            would_be: i64::MAX,
        })?;
    if would_be > leaf.amount_scaled {
        return Err(BatchError::OverClaim {
            position_id: leaf.position_id.clone(),
            committed: leaf.amount_scaled,
            would_be,
        });
    }
    Ok(would_be)
}

/// Renders a root as lowercase hex for the API/on-chain commitment.
pub fn root_hex(root: &Hash) -> String {
    to_hex(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instr(pos: &str, account: &str, amount: f64) -> SettlementInstruction {
        SettlementInstruction {
            position_id: pos.to_string(),
            account: account.to_string(),
            payout_asset: "USDC".to_string(),
            amount_scaled: (amount * 1e7).round() as i64,
        }
    }

    #[test]
    fn plan_rejects_duplicate_instructions() {
        let err = plan_batch(
            "USDC",
            vec![instr("p1", "GAA", 1.0), instr("p1", "GAA", 2.0)],
            30_000_000,
            1_000_000_000,
        )
        .unwrap_err();
        assert!(matches!(err, BatchError::DuplicateInstruction { .. }));
    }

    #[test]
    fn plan_rejects_conservation_breach() {
        let leaves = vec![instr("p1", "GAA", 1.0), instr("p2", "GBB", 2.0)];
        // Declared total wrong.
        assert!(matches!(
            plan_batch("USDC", leaves.clone(), 999, 1_000_000_000),
            Err(BatchError::ConservationViolated { .. })
        ));
        // Correct total but not covered by collateral.
        assert!(matches!(
            plan_batch("USDC", leaves, 30_000_000, 10_000_000),
            Err(BatchError::ConservationViolated { .. })
        ));
    }

    #[test]
    fn plan_accepts_balanced_batch() {
        let leaves = vec![instr("p1", "GAA", 1.0), instr("p2", "GBB", 2.0)];
        let batch = plan_batch("USDC", leaves, 30_000_000, 30_000_000).unwrap();
        assert_eq!(batch.commitment.leaf_count, 2);
        assert_eq!(batch.commitment.total_amount_scaled, 30_000_000);
    }

    #[test]
    fn plan_rejects_mixed_payout_assets() {
        let mut a = instr("p1", "GAA", 1.0);
        a.payout_asset = "EURC".to_string();
        let err = plan_batch(
            "USDC",
            vec![a, instr("p2", "GBB", 2.0)],
            30_000_000,
            1_000_000_000,
        )
        .unwrap_err();
        assert!(matches!(err, BatchError::InvalidState { .. }));
    }

    #[test]
    fn partial_claims_accumulate_up_to_the_committed_amount() {
        let leaf = instr("p1", "GAA", 10.0);
        let after_first = apply_claim(&leaf, 0, 40_000_000).unwrap();
        assert_eq!(after_first, 40_000_000);
        let after_second = apply_claim(&leaf, after_first, 60_000_000).unwrap();
        assert_eq!(after_second, 100_000_000);
        // One stroop more must fail.
        assert!(matches!(
            apply_claim(&leaf, after_second, 1),
            Err(BatchError::OverClaim { .. })
        ));
    }

    #[test]
    fn non_positive_claims_are_rejected() {
        let leaf = instr("p1", "GAA", 10.0);
        assert!(matches!(
            apply_claim(&leaf, 0, 0),
            Err(BatchError::NothingToClaim { .. })
        ));
    }

    #[test]
    fn root_hex_is_64_lowercase_hex_chars() {
        let batch = plan_batch(
            "USDC",
            vec![instr("p1", "GAA", 1.0)],
            10_000_000,
            10_000_000,
        )
        .unwrap();
        let hex = root_hex(&batch.commitment.root);
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
