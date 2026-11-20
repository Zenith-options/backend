//! HTTP handlers for series-adjustment CRUD and approval flow.
//!
//! All write endpoints require the `ZENITH_OPERATOR_TOKEN` header (same gate
//! as `series_lifecycle`).  Approval requires a *different* operator token
//! than the one that submitted (`approved_by != submitted_by` enforced at the
//! DB level by convention; the API records both wallets in the audit trail).
//!
//! Routes (all under /api/v1/admin/adjustments):
//!   POST   /                    – submit a new adjustment
//!   GET    /                    – list (filterable by underlying & status)
//!   GET    /:id                 – get one
//!   POST   /:id/approve         – approve (second operator)
//!   POST   /:id/reject          – reject
//!   POST   /:id/apply           – apply an approved adjustment
//!   GET    /:id/history         – per-series audit log

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppJson};
use crate::series_adjustment::{
    self, AdjustmentError, AdjustmentKind, SeriesAdjustment,
};
use crate::series_lifecycle::require_operator;
use crate::AppState;

// ─── Request shapes ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SubmitRequest {
    pub kind: String,
    pub underlying: String,
    pub ratio_numerator: Option<i64>,
    pub ratio_denominator: Option<i64>,
    pub new_sac_address: Option<String>,
    pub effective_at: String,
    pub effective_ledger: i64,
    pub reason: String,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub underlying: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct RejectRequest {
    pub reason: String,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

pub async fn post_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppJson(req): AppJson<SubmitRequest>,
) -> Result<(StatusCode, Json<SeriesAdjustment>), AppError> {
    let operator = require_operator_wallet(&headers)?;

    let kind = AdjustmentKind::from_str(&req.kind).ok_or_else(|| {
        AppError::new(StatusCode::BAD_REQUEST, format!("unknown kind \"{}\"", req.kind))
    })?;

    let n = req.ratio_numerator.unwrap_or(1);
    let d = req.ratio_denominator.unwrap_or(1);
    if n <= 0 || d <= 0 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "ratio must be positive"));
    }

    let adj = series_adjustment::submit_adjustment(
        &state.db, kind, &req.underlying,
        n, d,
        req.new_sac_address.as_deref(),
        &req.effective_at,
        req.effective_ledger,
        &req.reason,
        &operator,
    )
    .await
    .map_err(adj_err)?;

    Ok((StatusCode::CREATED, Json(adj)))
}

pub async fn get_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<SeriesAdjustment>>, AppError> {
    require_operator(&headers).map_err(|_| {
        AppError::new(StatusCode::UNAUTHORIZED, "operator token required")
    })?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let list = series_adjustment::list_adjustments(
        &state.db,
        q.underlying.as_deref(),
        q.status.as_deref(),
        limit,
        offset,
    )
    .await
    .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(list))
}

pub async fn get_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SeriesAdjustment>, AppError> {
    require_operator(&headers).map_err(|_| {
        AppError::new(StatusCode::UNAUTHORIZED, "operator token required")
    })?;
    let adj = series_adjustment::get_adjustment(&state.db, &id)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "adjustment not found"))?;
    Ok(Json(adj))
}

pub async fn post_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SeriesAdjustment>, AppError> {
    let operator = require_operator_wallet(&headers)?;
    let adj = series_adjustment::approve_adjustment(&state.db, &id, &operator)
        .await
        .map_err(adj_err)?;
    Ok(Json(adj))
}

pub async fn post_reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    AppJson(req): AppJson<RejectRequest>,
) -> Result<Json<SeriesAdjustment>, AppError> {
    let operator = require_operator_wallet(&headers)?;
    let adj = series_adjustment::reject_adjustment(&state.db, &id, &operator, &req.reason)
        .await
        .map_err(adj_err)?;
    Ok(Json(adj))
}

pub async fn post_apply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_operator(&headers).map_err(|_| {
        AppError::new(StatusCode::UNAUTHORIZED, "operator token required")
    })?;
    let history = series_adjustment::apply_adjustment(&state.db, &id)
        .await
        .map_err(adj_err)?;
    Ok(Json(serde_json::json!({
        "adjustment_id": id,
        "series_affected": history.len(),
        "history": history,
    })))
}

pub async fn get_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_operator(&headers).map_err(|_| {
        AppError::new(StatusCode::UNAUTHORIZED, "operator token required")
    })?;
    let rows = series_adjustment::list_history(&state.db, &id)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(serde_json::json!({ "adjustment_id": id, "history": rows })))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn require_operator_wallet(headers: &HeaderMap) -> Result<String, AppError> {
    require_operator(headers).map_err(|_| {
        AppError::new(StatusCode::UNAUTHORIZED, "operator token required")
    })?;
    // Extract a wallet identifier from the token header for the audit trail.
    // Fall back to a sentinel so the record is never blank.
    let wallet = headers
        .get("x-operator-wallet")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("operator")
        .to_string();
    Ok(wallet)
}

fn adj_err(e: AdjustmentError) -> AppError {
    match &e {
        AdjustmentError::NotFound(_) => {
            AppError::new(StatusCode::NOT_FOUND, format!("{e}"))
        }
        AdjustmentError::WrongStatus(_, _) => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AdjustmentError::ZeroStrike(_) | AdjustmentError::ZeroDenominator => {
            AppError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("{e}"))
        }
        AdjustmentError::SettlementInProgress(_) => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AdjustmentError::AlreadyApplied => {
            AppError::new(StatusCode::CONFLICT, "adjustment already applied")
        }
        AdjustmentError::Db(_) => {
            let msg = format!("{e}");
            AppError::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    }
}
