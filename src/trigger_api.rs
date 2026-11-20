//! HTTP handlers for trigger-order CRUD.
//!
//! Routes:
//!   POST   /api/v1/triggers              – arm a new trigger
//!   GET    /api/v1/triggers              – list (with optional ?status= filter)
//!   GET    /api/v1/triggers/:id          – get one
//!   POST   /api/v1/triggers/:id/cancel   – cancel an armed trigger
//!   PATCH  /api/v1/triggers/:id          – amend price/size

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{db_error, AppError, AppJson};
use crate::triggers::{
    amend_trigger, cancel_trigger, create_trigger, get_trigger, list_triggers,
    TriggerError, TriggerOrder, TriggerType,
};
use crate::AppState;

// ─── Request / response shapes ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateTriggerRequest {
    /// "stop_loss" | "take_profit" | "stop_limit"
    pub trigger_type: String,
    /// "close_long" | "close_short" | "open_long" | "open_short"
    pub side: String,
    /// Underlying symbol, e.g. "BTC"
    pub underlying: String,
    /// Optional position id to attach the trigger to.
    pub position_id: Option<String>,
    /// Trigger price in display units (not scaled).
    pub trigger_price: f64,
    /// Limit price for stop_limit orders.
    pub limit_price: Option<f64>,
    /// Size in contracts.
    pub size: f64,
    /// "gtc" | "ioc" | "fok"
    #[serde(default = "default_tif")]
    pub time_in_force: String,
    /// Max slippage in basis points (default 200 = 2%).
    #[serde(default = "default_slippage")]
    pub slippage_bps: i64,
    /// Optional ISO-8601 expiry; NULL = never expires.
    pub expires_at: Option<String>,
}

fn default_tif() -> String { "gtc".into() }
fn default_slippage() -> i64 { 200 }

#[derive(Deserialize)]
pub struct ListTriggersQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct AmendTriggerRequest {
    pub trigger_price: Option<f64>,
    pub limit_price: Option<f64>,
    pub size: Option<f64>,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

pub async fn post_create_trigger(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    AppJson(req): AppJson<CreateTriggerRequest>,
) -> Result<(StatusCode, Json<TriggerOrder>), AppError> {
    let ttype = TriggerType::from_str(&req.trigger_type).ok_or_else(|| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            format!("unknown trigger_type \"{}\"", req.trigger_type),
        )
    })?;

    if req.trigger_price <= 0.0 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "trigger_price must be positive"));
    }
    if req.size <= 0.0 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "size must be positive"));
    }
    if !["gtc", "ioc", "fok"].contains(&req.time_in_force.as_str()) {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "time_in_force must be gtc | ioc | fok"));
    }
    if !state.spot_prices.lock().unwrap().contains_key(&req.underlying) {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            format!("unknown underlying \"{}\"", req.underlying),
        ));
    }

    let trigger = create_trigger(
        &state.db,
        &wallet,
        ttype,
        &req.side,
        &req.underlying,
        req.position_id.as_deref(),
        req.trigger_price,
        req.limit_price,
        req.size,
        &req.time_in_force,
        req.slippage_bps,
        req.expires_at.as_deref(),
    )
    .await
    .map_err(trigger_to_app_error)?;

    Ok((StatusCode::CREATED, Json(trigger)))
}

pub async fn get_list_triggers(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    Query(q): Query<ListTriggersQuery>,
) -> Result<Json<Vec<TriggerOrder>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let triggers = list_triggers(&state.db, &wallet, q.status.as_deref(), limit, offset)
        .await
        .map_err(|e| db_error("list triggers", e))?;

    Ok(Json(triggers))
}

pub async fn get_one_trigger(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    Path(id): Path<String>,
) -> Result<Json<TriggerOrder>, AppError> {
    let trigger = get_trigger(&state.db, &id)
        .await
        .map_err(|e| db_error("get trigger", e))?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "trigger not found"))?;

    if trigger.wallet_address != wallet {
        return Err(AppError::new(StatusCode::FORBIDDEN, "not your trigger"));
    }

    Ok(Json(trigger))
}

pub async fn post_cancel_trigger(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    Path(id): Path<String>,
) -> Result<Json<TriggerOrder>, AppError> {
    let trigger = cancel_trigger(&state.db, &id, &wallet)
        .await
        .map_err(trigger_to_app_error)?;
    Ok(Json(trigger))
}

pub async fn patch_amend_trigger(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    Path(id): Path<String>,
    AppJson(req): AppJson<AmendTriggerRequest>,
) -> Result<Json<TriggerOrder>, AppError> {
    if req.trigger_price.is_none() && req.limit_price.is_none() && req.size.is_none() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "provide at least one of trigger_price, limit_price, or size",
        ));
    }

    let trigger = amend_trigger(
        &state.db,
        &id,
        &wallet,
        req.trigger_price,
        req.limit_price,
        req.size,
    )
    .await
    .map_err(trigger_to_app_error)?;

    Ok(Json(trigger))
}

// ─── Error mapping ────────────────────────────────────────────────────────────

fn trigger_to_app_error(e: TriggerError) -> AppError {
    match &e {
        TriggerError::NotFound(id) => {
            AppError::new(StatusCode::NOT_FOUND, format!("trigger {id} not found"))
        }
        TriggerError::NotArmed(s) => {
            AppError::new(StatusCode::CONFLICT, format!("trigger is not armed (status={s})"))
        }
        TriggerError::Forbidden(id) => {
            AppError::new(StatusCode::FORBIDDEN, format!("not your trigger ({id})"))
        }
        TriggerError::Db(_) => db_error("trigger operation", match e { TriggerError::Db(inner) => inner, _ => unreachable!() }),
        TriggerError::SlippageTooWide { .. } => {
            AppError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("{e}"))
        }
        TriggerError::InvalidField(f) => {
            AppError::new(StatusCode::BAD_REQUEST, format!("invalid amendment field: {f}"))
        }
    }
}
