//! HTTP handlers for the liquidation auction API.
//!
//! Routes:
//!   POST   /api/v1/auction/lots                  – create a lot (operator)
//!   GET    /api/v1/auction/lots                   – list lots
//!   GET    /api/v1/auction/lots/:id               – get one lot
//!   POST   /api/v1/auction/lots/:id/reserve       – update reserve (operator)
//!   POST   /api/v1/auction/lots/:id/bids          – submit a sealed bid
//!   POST   /api/v1/auction/bids/:id/reveal         – reveal a bid
//!   GET    /api/v1/auction/lots/:id/bids          – list bids for a lot
//!   POST   /api/v1/auction/lots/:id/settle        – close+settle (operator)
//!   GET    /api/v1/auction/lots/:id/bonds         – bond-movement audit trail

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::chain::liquidation::auction::{
    self, close_and_settle, create_lot, get_lot, list_bids, list_bond_movements, list_lots,
    reveal_bid, submit_bid, update_reserve, AuctionBid, AuctionConfig, AuctionError, AuctionLot,
    BondMovement, DefaultFallback, NoopAuctionSettler,
};
use crate::error::{AppError, AppJson};
use crate::AppState;

// ─── Request shapes ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateLotRequest {
    pub liquidation_id: String,
    pub collateral_asset: String,
    pub quantity: i64,
    pub reserve_price: i64,
    pub settlement_asset: String,
    pub covered_positions: Vec<String>,
    pub open_ledger: i64,
}

#[derive(Deserialize)]
pub struct UpdateReserveRequest {
    pub reserve_price: i64,
}

#[derive(Deserialize)]
pub struct SubmitBidRequest {
    pub bidder: String,
    pub commitment: String,
    pub bond_amount: i64,
    pub current_ledger: i64,
}

#[derive(Deserialize)]
pub struct RevealBidRequest {
    pub revealed_amount: i64,
    pub nonce: String,
    pub current_ledger: i64,
}

#[derive(Deserialize)]
pub struct SettleRequest {
    pub current_ledger: i64,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

pub async fn post_create_lot(
    State(state): State<AppState>,
    AppJson(req): AppJson<CreateLotRequest>,
) -> Result<(StatusCode, Json<AuctionLot>), AppError> {
    if req.quantity <= 0 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "quantity must be positive"));
    }
    let lot = create_lot(
        &state.db,
        &req.liquidation_id,
        &req.collateral_asset,
        req.quantity,
        req.reserve_price,
        &req.settlement_asset,
        &req.covered_positions,
        req.open_ledger,
        &AuctionConfig::default(),
        "operator",
    )
    .await
    .map_err(auction_err)?;
    Ok((StatusCode::CREATED, Json(lot)))
}

pub async fn get_list_lots(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<AuctionLot>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let lots = list_lots(&state.db, q.status.as_deref(), limit, offset)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(lots))
}

pub async fn get_one_lot(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AuctionLot>, AppError> {
    let lot = get_lot(&state.db, &id)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "lot not found"))?;
    Ok(Json(lot))
}

pub async fn post_update_reserve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    AppJson(req): AppJson<UpdateReserveRequest>,
) -> Result<Json<AuctionLot>, AppError> {
    let lot = update_reserve(&state.db, &id, req.reserve_price)
        .await
        .map_err(auction_err)?;
    Ok(Json(lot))
}

pub async fn post_submit_bid(
    State(state): State<AppState>,
    Path(id): Path<String>,
    AppJson(req): AppJson<SubmitBidRequest>,
) -> Result<(StatusCode, Json<AuctionBid>), AppError> {
    let bid = submit_bid(
        &state.db,
        &id,
        &req.bidder,
        &req.commitment,
        req.bond_amount,
        req.current_ledger,
    )
    .await
    .map_err(auction_err)?;
    Ok((StatusCode::CREATED, Json(bid)))
}

pub async fn post_reveal_bid(
    State(state): State<AppState>,
    Path(id): Path<String>,
    AppJson(req): AppJson<RevealBidRequest>,
) -> Result<Json<AuctionBid>, AppError> {
    let bid = reveal_bid(&state.db, &id, req.revealed_amount, &req.nonce, req.current_ledger)
        .await
        .map_err(auction_err)?;
    Ok(Json(bid))
}

pub async fn get_list_bids(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<AuctionBid>>, AppError> {
    let bids = list_bids(&state.db, &id)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(bids))
}

pub async fn post_settle(
    State(state): State<AppState>,
    Path(id): Path<String>,
    AppJson(req): AppJson<SettleRequest>,
) -> Result<Json<AuctionLot>, AppError> {
    let lot = close_and_settle(
        &state.db,
        &id,
        req.current_ledger,
        &NoopAuctionSettler,
        &DefaultFallback,
    )
    .await
    .map_err(auction_err)?;
    Ok(Json(lot))
}

pub async fn get_bonds(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<BondMovement>>, AppError> {
    let movements = list_bond_movements(&state.db, &id)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(movements))
}

// ─── Error mapping ───────────────────────────────────────────────────────────

fn auction_err(e: AuctionError) -> AppError {
    match &e {
        AuctionError::NotFound(_) => AppError::new(StatusCode::NOT_FOUND, format!("{e}")),
        AuctionError::WrongStatus(_, _) => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AuctionError::BiddingClosed(_) => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AuctionError::ReserveLocked => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AuctionError::InsufficientBond => {
            AppError::new(StatusCode::BAD_REQUEST, format!("{e}"))
        }
        AuctionError::NoWinningBid => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AuctionError::AlreadySettled => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AuctionError::UnderlyingPaused => {
            AppError::new(StatusCode::CONFLICT, format!("{e}"))
        }
        AuctionError::Db(_) => {
            AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))
        }
    }
}
