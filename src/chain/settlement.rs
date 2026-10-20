//! Persistence and HTTP surface for Merkle-commitment batch settlement.
//!
//! The pure logic lives in [`super::batch`] and [`super::merkle`]; this module
//! is the thin, testable layer that stores a batch and its leaves, drives the
//! lifecycle, and serves proofs/claims. Every DB write here uses runtime
//! `sqlx::query` (not the macros) so no offline query cache is required.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use validator::Validate;

use crate::auth::AuthUser;
use crate::error::{db_error, AppError, ValidatedJson};
use crate::AppState;

use super::batch::{
    apply_claim, check_conservation, root_hex, BatchError, BatchStatus, PlannedBatch,
    SettlementInstruction,
};

/// Maps a domain error onto a stable HTTP status + error code.
fn map_batch_error(err: BatchError) -> AppError {
    let (status, code) = match &err {
        BatchError::BatchNotFound { .. } | BatchError::LeafNotFound { .. } => {
            (StatusCode::NOT_FOUND, "not_found")
        }
        BatchError::DuplicateInstruction { .. }
        | BatchError::ConservationViolated { .. }
        | BatchError::Merkle(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_batch"),
        BatchError::LeafSetLocked => (StatusCode::CONFLICT, "leaf_set_locked"),
        BatchError::OverClaim { .. } => (StatusCode::CONFLICT, "over_claim"),
        BatchError::NothingToClaim { .. } => (StatusCode::CONFLICT, "nothing_to_claim"),
        BatchError::InvalidState { .. } => (StatusCode::CONFLICT, "invalid_state"),
    };
    AppError::coded(status, code, err.to_string(), None)
}

#[derive(Debug, Deserialize, Validate)]
pub struct PlanBatchRequest {
    #[validate(length(min = 1, max = 64))]
    pub payout_asset: String,
    /// Sum of all leaf amounts, in stroops. Must equal the leaves' sum and be
    /// no larger than the operator's available collateral.
    #[validate(range(min = 1))]
    pub total_scaled: i64,
    #[validate(length(min = 1, max = 10_000))]
    pub instructions: Vec<InstructionInput>,
}

#[derive(Debug, Deserialize, Serialize, Validate)]
pub struct InstructionInput {
    #[validate(length(min = 1, max = 128))]
    pub position_id: String,
    #[validate(length(min = 1, max = 128))]
    pub account: String,
    #[validate(range(min = 0))]
    pub amount_scaled: i64,
}

impl InstructionInput {
    fn into_instruction(self, payout_asset: &str) -> SettlementInstruction {
        SettlementInstruction {
            position_id: self.position_id,
            account: self.account,
            payout_asset: payout_asset.to_string(),
            amount_scaled: self.amount_scaled,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub batch_id: String,
    pub payout_asset: String,
    pub status: String,
    pub root: Option<String>,
    pub leaf_count: Option<i64>,
    pub leaf_version: i64,
    pub total_scaled: i64,
    pub available_scaled: i64,
    pub committed_tx_hash: Option<String>,
    pub created_at: String,
    pub committed_at: Option<String>,
    pub finalized_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LeafResponse {
    pub position_id: String,
    pub account: String,
    pub amount_scaled: i64,
    pub claimed_scaled: i64,
    pub leaf_hash: String,
}

#[derive(Debug, Serialize)]
pub struct ProofResponse {
    pub batch_id: String,
    pub position_id: String,
    pub account: String,
    pub leaf_version: u32,
    pub leaf: String,
    pub root: String,
    pub proof: Vec<ProofStepResponse>,
}

#[derive(Debug, Serialize)]
pub struct ProofStepResponse {
    pub sibling: String,
    pub sibling_is_right: bool,
}

#[derive(Debug, Deserialize, Validate)]
pub struct CommitBatchRequest {
    /// Hash of the single signed transaction carrying the commitment.
    #[validate(length(min = 1, max = 128))]
    pub commitment_tx_hash: String,
    pub commitment_ledger: Option<i64>,
}

#[derive(Debug, Deserialize, Validate)]
pub struct ClaimRequest {
    #[validate(length(min = 1, max = 128))]
    pub position_id: String,
    #[validate(range(min = 1))]
    pub amount_scaled: i64,
    /// Replaying the same key returns the original claim without moving money
    /// again.
    #[validate(length(min = 1, max = 128))]
    pub idempotency_key: String,
}

#[derive(Debug, Serialize)]
pub struct ClaimResponse {
    pub batch_id: String,
    pub position_id: String,
    pub account: String,
    pub amount_scaled: i64,
    pub claimed_total_scaled: i64,
    pub outstanding_scaled: i64,
    pub idempotent_replay: bool,
}

fn row_to_batch(row: &sqlx::sqlite::SqliteRow) -> BatchResponse {
    BatchResponse {
        batch_id: row.get("id"),
        payout_asset: row.get("payout_asset"),
        status: row.get("status"),
        root: row.get("root"),
        leaf_count: row.get("leaf_count"),
        leaf_version: row.get("leaf_version"),
        total_scaled: row.get("total_scaled"),
        available_scaled: row.get("available_scaled"),
        committed_tx_hash: row.get("commitment_tx_hash"),
        created_at: row.get("created_at"),
        committed_at: row.get("committed_at"),
        finalized_at: row.get("finalized_at"),
    }
}

const BATCH_COLUMNS: &str = "id, payout_asset, status, root, leaf_count, leaf_version, \
     total_scaled, available_scaled, commitment_tx_hash, created_at, committed_at, finalized_at";

async fn load_batch(state: &AppState, batch_id: &str) -> Result<BatchResponse, AppError> {
    let sql = format!("SELECT {BATCH_COLUMNS} FROM settlement_batches WHERE id = ?");
    let row = sqlx::query(&sql)
        .bind(batch_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| db_error("load settlement batch", e))?
        .ok_or_else(|| {
            map_batch_error(BatchError::BatchNotFound {
                batch_id: batch_id.to_string(),
            })
        })?;
    Ok(row_to_batch(&row))
}

/// Loads the stored leaves for a batch as instructions, in canonical order.
async fn load_leaves(
    state: &AppState,
    batch_id: &str,
    payout_asset: &str,
) -> Result<Vec<SettlementInstruction>, AppError> {
    let rows = sqlx::query(
        "SELECT position_id, account, amount_scaled
           FROM settlement_batch_leaves WHERE batch_id = ?",
    )
    .bind(batch_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("load settlement leaves", e))?;
    let mut instructions: Vec<SettlementInstruction> = rows
        .iter()
        .map(|r| SettlementInstruction {
            position_id: r.get("position_id"),
            account: r.get("account"),
            amount_scaled: r.get("amount_scaled"),
            payout_asset: payout_asset.to_string(),
        })
        .collect();
    instructions.sort_by(|a, b| (&a.position_id, &a.account).cmp(&(&b.position_id, &b.account)));
    Ok(instructions)
}

/// `POST /api/v1/settlement/batches` — plan a batch (PLANNED state).
pub async fn plan_batch(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    ValidatedJson(req): ValidatedJson<PlanBatchRequest>,
) -> Result<(StatusCode, Json<BatchResponse>), AppError> {
    // The operator's own account balance is the pool collateral the batch is
    // checked against: a batch can never promise more than the operator holds.
    let available: i64 =
        sqlx::query_scalar("SELECT COALESCE(balance, 0) FROM accounts WHERE wallet_address = ?")
            .bind(&wallet_address)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| db_error("load operator balance", e))?
            .unwrap_or(0);

    let instructions: Vec<SettlementInstruction> = req
        .instructions
        .into_iter()
        .map(|i| i.into_instruction(&req.payout_asset))
        .collect();

    let planned = super::batch::plan_batch(
        req.payout_asset.clone(),
        instructions,
        req.total_scaled,
        available,
    )
    .map_err(map_batch_error)?;

    persist_planned(&state, &planned).await
}

async fn persist_planned(
    state: &AppState,
    planned: &PlannedBatch,
) -> Result<(StatusCode, Json<BatchResponse>), AppError> {
    let tree = super::batch::build_tree(&planned.instructions).map_err(map_batch_error)?;
    let batch_id = uuid::Uuid::new_v4().to_string();
    let total = planned.commitment.total_amount_scaled;

    let mut tx = state.db.begin().await.map_err(|e| db_error("begin", e))?;
    sqlx::query(
        "INSERT INTO settlement_batches
            (id, payout_asset, status, leaf_version, total_scaled, available_scaled)
         VALUES (?, ?, 'planned', ?, ?, ?)",
    )
    .bind(&batch_id)
    .bind(&planned.payout_asset)
    .bind(planned.commitment.leaf_version as i64)
    .bind(total)
    .bind(planned.available_scaled)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_error("insert settlement batch", e))?;

    for leaf in tree.leaves() {
        sqlx::query(
            "INSERT INTO settlement_batch_leaves
                (batch_id, position_id, account, amount_scaled, leaf_hash)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&batch_id)
        .bind(&leaf.position_id)
        .bind(&leaf.account)
        .bind(leaf.amount_scaled)
        .bind(root_hex(&leaf.hash()))
        .execute(&mut *tx)
        .await
        .map_err(|e| db_error("insert settlement leaf", e))?;
    }

    tx.commit().await.map_err(|e| db_error("commit", e))?;

    // `available_scaled` was written by the caller in the INSERT; reload to
    // return the true stored row.
    let batch = load_batch(state, &batch_id).await?;
    Ok((StatusCode::CREATED, Json(batch)))
}

/// `POST /api/v1/settlement/batches/:id/commit` — commit the root on chain.
///
/// Recomputes the tree from the persisted leaves and requires the supplied
/// root (if given) to match; sets status to COMMITTED, which locks the leaf
/// set via the DB triggers. Idempotent: committing an already-committed batch
/// with the same transaction hash returns the existing batch.
pub async fn commit_batch(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    AuthUser(_wallet): AuthUser,
    ValidatedJson(req): ValidatedJson<CommitBatchRequest>,
) -> Result<Json<BatchResponse>, AppError> {
    let batch = load_batch(&state, &batch_id).await?;
    let status = BatchStatus::parse(&batch.status).ok_or_else(|| {
        map_batch_error(BatchError::InvalidState {
            expected: "planned",
            actual: batch.status.clone(),
        })
    })?;

    if status == BatchStatus::Committed || status == BatchStatus::Finalized {
        // Idempotent replay of the same commitment.
        if batch.committed_tx_hash.as_deref() == Some(req.commitment_tx_hash.as_str()) {
            return Ok(Json(batch));
        }
        return Err(map_batch_error(BatchError::LeafSetLocked));
    }

    let leaves = load_leaves(&state, &batch_id, &batch.payout_asset).await?;
    if leaves.is_empty() {
        return Err(map_batch_error(BatchError::InvalidState {
            expected: "non-empty leaf set",
            actual: "empty".to_string(),
        }));
    }
    // Re-verify conservation against the persisted numbers.
    check_conservation(&leaves, batch.total_scaled, batch.available_scaled)
        .map_err(map_batch_error)?;
    let tree = super::batch::build_tree(&leaves).map_err(map_batch_error)?;
    let commitment = tree.commitment();

    sqlx::query(
        "UPDATE settlement_batches
            SET status = 'committed', root = ?, leaf_count = ?,
                commitment_tx_hash = ?, commitment_ledger = ?,
                committed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE id = ? AND status = 'planned'",
    )
    .bind(root_hex(&commitment.root))
    .bind(commitment.leaf_count as i64)
    .bind(&req.commitment_tx_hash)
    .bind(req.commitment_ledger)
    .bind(&batch_id)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("commit settlement batch", e))?;

    let batch = load_batch(&state, &batch_id).await?;
    Ok(Json(batch))
}

/// `GET /api/v1/settlement/batches/:id`
pub async fn get_batch(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    AuthUser(_wallet): AuthUser,
) -> Result<Json<BatchResponse>, AppError> {
    Ok(Json(load_batch(&state, &batch_id).await?))
}

/// `GET /api/v1/settlement/batches/:id/leaves`
pub async fn list_leaves(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    AuthUser(_wallet): AuthUser,
) -> Result<Json<Vec<LeafResponse>>, AppError> {
    let _ = load_batch(&state, &batch_id).await?;
    let rows = sqlx::query(
        "SELECT position_id, account, amount_scaled, claimed_scaled, leaf_hash
           FROM settlement_batch_leaves WHERE batch_id = ?
          ORDER BY position_id, account",
    )
    .bind(&batch_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("load settlement leaves", e))?;
    let leaves = rows
        .iter()
        .map(|r| LeafResponse {
            position_id: r.get("position_id"),
            account: r.get("account"),
            amount_scaled: r.get("amount_scaled"),
            claimed_scaled: r.get("claimed_scaled"),
            leaf_hash: r.get("leaf_hash"),
        })
        .collect();
    Ok(Json(leaves))
}

/// `GET /api/v1/settlement/batches/:id/leaves/:position_id/proof`
pub async fn get_proof(
    State(state): State<AppState>,
    Path((batch_id, position_id)): Path<(String, String)>,
    AuthUser(_wallet): AuthUser,
) -> Result<Json<ProofResponse>, AppError> {
    let batch = load_batch(&state, &batch_id).await?;
    let root = batch.root.clone().unwrap_or_default();
    let leaves = load_leaves(&state, &batch_id, &batch.payout_asset).await?;
    let tree = super::batch::build_tree(&leaves).map_err(map_batch_error)?;

    // The account is not part of the URL, so find the (unique) leaf for this
    // position id.
    let leaf = tree
        .leaves()
        .iter()
        .find(|l| l.position_id == position_id)
        .ok_or_else(|| {
            map_batch_error(BatchError::LeafNotFound {
                position_id: position_id.clone(),
            })
        })?;
    let proof = tree
        .proof_for(&leaf.position_id, &leaf.account)
        .ok_or_else(|| {
            map_batch_error(BatchError::LeafNotFound {
                position_id: position_id.clone(),
            })
        })?;

    Ok(Json(ProofResponse {
        batch_id,
        position_id: leaf.position_id.clone(),
        account: leaf.account.clone(),
        leaf_version: proof.leaf_version,
        leaf: root_hex(&proof.leaf),
        root,
        proof: proof
            .steps
            .iter()
            .map(|s| ProofStepResponse {
                sibling: root_hex(&s.sibling),
                sibling_is_right: s.sibling_is_right,
            })
            .collect(),
    }))
}

/// `POST /api/v1/settlement/batches/:id/claims` — claim (fully or partially).
///
/// Idempotent on `(batch_id, position_id, idempotency_key)`. A leaf may be
/// claimed repeatedly as long as the running total never exceeds its committed
/// amount; when every leaf is fully claimed the batch is marked FINALIZED.
pub async fn claim(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    AuthUser(wallet_address): AuthUser,
    ValidatedJson(req): ValidatedJson<ClaimRequest>,
) -> Result<Json<ClaimResponse>, AppError> {
    let batch = load_batch(&state, &batch_id).await?;
    let status = BatchStatus::parse(&batch.status).ok_or_else(|| {
        map_batch_error(BatchError::InvalidState {
            expected: "committed",
            actual: batch.status.clone(),
        })
    })?;
    if status == BatchStatus::Planned {
        return Err(map_batch_error(BatchError::InvalidState {
            expected: "committed",
            actual: batch.status.clone(),
        }));
    }
    if status == BatchStatus::Finalized {
        return Err(map_batch_error(BatchError::InvalidState {
            expected: "committed",
            actual: batch.status.clone(),
        }));
    }

    let mut tx = state.db.begin().await.map_err(|e| db_error("begin", e))?;

    // Idempotency: return the original claim if the key was already used.
    if let Some(existing) = sqlx::query(
        "SELECT account, amount_scaled FROM settlement_claims
          WHERE batch_id = ? AND position_id = ? AND idempotency_key = ?",
    )
    .bind(&batch_id)
    .bind(&req.position_id)
    .bind(&req.idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| db_error("lookup claim", e))?
    {
        let account: String = existing.get("account");
        let amount: i64 = existing.get("amount_scaled");
        let claimed_total = leaf_claimed(&mut tx, &batch_id, &req.position_id).await?;
        let committed = leaf_amount(&mut tx, &batch_id, &req.position_id).await?;
        tx.commit().await.map_err(|e| db_error("commit", e))?;
        return Ok(Json(ClaimResponse {
            batch_id,
            position_id: req.position_id,
            account,
            amount_scaled: amount,
            claimed_total_scaled: claimed_total,
            outstanding_scaled: committed.saturating_sub(claimed_total),
            idempotent_replay: true,
        }));
    }

    // Load the leaf row (locked for update within this transaction).
    let row = sqlx::query(
        "SELECT account, amount_scaled, claimed_scaled FROM settlement_batch_leaves
          WHERE batch_id = ? AND position_id = ?",
    )
    .bind(&batch_id)
    .bind(&req.position_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| db_error("load settlement leaf", e))?
    .ok_or_else(|| {
        map_batch_error(BatchError::LeafNotFound {
            position_id: req.position_id.clone(),
        })
    })?;

    let account: String = row.get("account");
    let committed: i64 = row.get("amount_scaled");
    let already: i64 = row.get("claimed_scaled");

    // The claimant must be the leaf's own account (the payout recipient).
    if account != wallet_address {
        return Err(AppError::coded(
            StatusCode::FORBIDDEN,
            "not_claimant",
            "only the payout account may claim this leaf".to_string(),
            None,
        ));
    }

    let leaf = SettlementInstruction {
        position_id: req.position_id.clone(),
        account: account.clone(),
        payout_asset: batch.payout_asset.clone(),
        amount_scaled: committed,
    };
    let new_total = apply_claim(&leaf, already, req.amount_scaled).map_err(map_batch_error)?;

    sqlx::query(
        "INSERT INTO settlement_claims
            (batch_id, position_id, account, amount_scaled, idempotency_key)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&batch_id)
    .bind(&req.position_id)
    .bind(&account)
    .bind(req.amount_scaled)
    .bind(&req.idempotency_key)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_error("insert claim", e))?;

    sqlx::query(
        "UPDATE settlement_batch_leaves
            SET claimed_scaled = ?, claimed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE batch_id = ? AND position_id = ?",
    )
    .bind(new_total)
    .bind(&batch_id)
    .bind(&req.position_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_error("update settlement leaf", e))?;

    // If every leaf is now fully claimed, finalize the batch.
    let outstanding: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM settlement_batch_leaves
          WHERE batch_id = ? AND claimed_scaled < amount_scaled",
    )
    .bind(&batch_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| db_error("count outstanding leaves", e))?;
    if outstanding == 0 {
        sqlx::query(
            "UPDATE settlement_batches
                SET status = 'finalized',
                    finalized_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?",
        )
        .bind(&batch_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_error("finalize settlement batch", e))?;
    }

    tx.commit().await.map_err(|e| db_error("commit", e))?;

    Ok(Json(ClaimResponse {
        batch_id,
        position_id: req.position_id,
        account,
        amount_scaled: req.amount_scaled,
        claimed_total_scaled: new_total,
        outstanding_scaled: committed.saturating_sub(new_total),
        idempotent_replay: false,
    }))
}

async fn leaf_claimed(
    tx: &mut sqlx::SqliteConnection,
    batch_id: &str,
    position_id: &str,
) -> Result<i64, AppError> {
    let value: i64 = sqlx::query_scalar(
        "SELECT COALESCE(claimed_scaled, 0) FROM settlement_batch_leaves
          WHERE batch_id = ? AND position_id = ?",
    )
    .bind(batch_id)
    .bind(position_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| db_error("load claimed", e))?
    .unwrap_or(0);
    Ok(value)
}

async fn leaf_amount(
    tx: &mut sqlx::SqliteConnection,
    batch_id: &str,
    position_id: &str,
) -> Result<i64, AppError> {
    let value: i64 = sqlx::query_scalar(
        "SELECT COALESCE(amount_scaled, 0) FROM settlement_batch_leaves
          WHERE batch_id = ? AND position_id = ?",
    )
    .bind(batch_id)
    .bind(position_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| db_error("load leaf amount", e))?
    .unwrap_or(0);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_batch_error_picks_sensible_statuses() {
        assert_eq!(
            map_batch_error(BatchError::BatchNotFound {
                batch_id: "x".into()
            })
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_batch_error(BatchError::LeafSetLocked).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            map_batch_error(BatchError::OverClaim {
                position_id: "p".into(),
                committed: 1,
                would_be: 2
            })
            .status,
            StatusCode::CONFLICT
        );
    }
}
