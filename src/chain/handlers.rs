//! HTTP handlers: POST /api/v1/tx/prepare, POST /api/v1/tx/submit, GET /api/v1/tx/:handle
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::auth::AuthUser;
use crate::chain::prepared_tx::{
    build_unsigned_envelope, current_ledger, load_pending_preparation, mark_used,
    store_preparation, validate_signed_envelope, PrepareRequest, PrepareResponse,
    PreparedTxError, SubmitRequest, SubmitResponse,
};
use crate::chain::submit::TxSubmitter;
use crate::error::{AppError, AppJson};
use crate::AppState;

impl From<PreparedTxError> for AppError {
    fn from(e: PreparedTxError) -> Self {
        let status = match &e {
            PreparedTxError::NotFound                    => StatusCode::NOT_FOUND,
            PreparedTxError::AlreadyUsed                 => StatusCode::CONFLICT,
            PreparedTxError::Expired                     => StatusCode::GONE,
            PreparedTxError::WrongSigner
            | PreparedTxError::EnvelopeTampered
            | PreparedTxError::NetworkPassphraseMismatch => StatusCode::UNPROCESSABLE_ENTITY,
            PreparedTxError::ExtraOperations
            | PreparedTxError::FeeBumpNotAllowed
            | PreparedTxError::MalformedEnvelope(_)
            | PreparedTxError::InvalidOperation(_)       => StatusCode::BAD_REQUEST,
            PreparedTxError::RpcError(_)
            | PreparedTxError::Db(_)                     => StatusCode::INTERNAL_SERVER_ERROR,
        };
        AppError::new(status, e.to_string())
    }
}

/// POST /api/v1/tx/prepare
///
/// Build an unsigned Soroban envelope for a vault/position operation.
/// Returns: unsigned XDR, opaque handle, operation type, expiration ledger,
/// human-readable summary. Single-use; expires in ~5 min (60 ledgers).
pub async fn post_prepare(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    AppJson(req): AppJson<PrepareRequest>,
) -> Result<Json<PrepareResponse>, AppError> {
    let ledger = current_ledger().await;
    let (unsigned_xdr, canonical_hash, summary, sequence) =
        build_unsigned_envelope(&wallet, &req, 100, ledger)
            .await
            .map_err(AppError::from)?;

    let expiration = ledger + 60;
    let handle = store_preparation(
        &state.db, &wallet, &req.operation, &unsigned_xdr,
        &canonical_hash, &wallet, sequence, expiration, &summary,
    )
    .await
    .map_err(AppError::from)?;

    Ok(Json(PrepareResponse { handle, unsigned_xdr, operation: req.operation, expiration_ledger: expiration, summary }))
}

/// POST /api/v1/tx/submit
///
/// Validate the wallet-signed envelope against the stored canonical hash
/// (op, contract, args, source, fee, TTL — all unchanged), then submit to RPC.
/// Rejects: tampered envelope, extra ops, fee-bump, wrong signer, replay, expiry.
pub async fn post_submit(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    AppJson(req): AppJson<SubmitRequest>,
) -> Result<Json<SubmitResponse>, AppError> {
    let ledger = current_ledger().await;
    let prep = load_pending_preparation(&state.db, &req.handle, &wallet, ledger)
        .await.map_err(AppError::from)?;

    validate_signed_envelope(&req.signed_xdr, &prep.canonical_hash, &wallet, &prep.operation)
        .map_err(AppError::from)?;

    let tx_hash = TxSubmitter::submit_transaction(&req.signed_xdr)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("RPC error: {e:?}")))?;

    let _ = mark_used(&state.db, &req.handle, &tx_hash).await;

    Ok(Json(SubmitResponse { tx_hash, status: "PENDING".to_string() }))
}

/// GET /api/v1/tx/:handle — look up a preparation (owning wallet only).
pub async fn get_preparation(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    Path(handle): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ledger = current_ledger().await;
    let prep = load_pending_preparation(&state.db, &handle, &wallet, ledger)
        .await.map_err(AppError::from)?;

    Ok(Json(serde_json::json!({
        "handle":            prep.handle,
        "operation":         prep.operation,
        "summary":           prep.summary,
        "expiration_ledger": prep.expiration_ledger,
        "status":            prep.status,
        "tx_hash":           prep.tx_hash,
        "created_at":        prep.created_at,
    })))
}
