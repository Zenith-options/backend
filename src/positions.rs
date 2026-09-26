use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Json;
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, Transaction};

use crate::auth::AuthUser;
use crate::collateral::collateral_required;
use crate::error::{db_error, AppError, AppJson, AppQuery};
use crate::margin::{MarginModel, RiskArrayMargin, StrategyBasedMargin};
use crate::models::{Account, Position};
use crate::{black_scholes, smile_vol, AppState, BSInputs, BSResult};

/// Selects the margin model for the current environment. `RiskArrayMargin`
/// (the SPAN-style stress grid) is the default; `StrategyBasedMargin` keeps
/// the legacy per-leg rules available as a fallback via the
/// `MARGIN_MODEL=strategy_based` environment flag.
pub(crate) fn margin_model() -> Box<dyn MarginModel> {
    match std::env::var("MARGIN_MODEL").as_deref() {
        Ok("strategy_based") => Box::new(StrategyBasedMargin),
        _ => Box::new(RiskArrayMargin::default()),
    }
}

pub async fn get_account(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Account>, AppError> {
    // Verify/login already creates this row, but stay defensive in case a
    // session outlives some future account-deletion path.
    sqlx::query(
        "INSERT INTO accounts (wallet_address) VALUES (?) ON CONFLICT(wallet_address) DO NOTHING",
    )
    .bind(&wallet_address)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("create or confirm account", e))?;

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(&wallet_address)
        .fetch_one(&state.db)
        .await
        .map_err(|e| db_error("load account", e))?;

    Ok(Json(account))
}

pub const DEFAULT_LIST_LIMIT: i64 = 50;
pub const MAX_LIST_LIMIT: i64 = 200;

#[derive(Deserialize)]
pub struct ListPositionsQuery {
    /// "open" | "closed" | "rolled" — omit to return every status.
    pub status: Option<String>,
    /// Restrict to the legs of one multi-leg strategy — omit for everything.
    pub strategy_id: Option<String>,
    /// Defaults to DEFAULT_LIST_LIMIT, capped at MAX_LIST_LIMIT regardless
    /// of what the caller asks for.
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Response shape is still a bare JSON array (unchanged, since the
/// frontend already consumes it that way) — total count and whether more
/// pages exist ride along as `X-Total-Count`/`X-Has-More` response
/// headers instead, the same pattern GitHub's API uses for exactly this
/// reason: it lets pagination metadata arrive without breaking existing
/// callers that expect the body to just be the list.
pub async fn list_positions(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    AppQuery(q): AppQuery<ListPositionsQuery>,
) -> Result<(HeaderMap, Json<Vec<Position>>), AppError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let offset = q.offset.unwrap_or(0).max(0);

    // `? IS NULL OR column = ?` lets one query handle all four
    // status/strategy_id filter combinations without branching SQL.
    let positions: Vec<Position> = sqlx::query_as(
        "SELECT * FROM positions
            WHERE wallet_address = ?
              AND (? IS NULL OR status = ?)
              AND (? IS NULL OR strategy_id = ?)
         ORDER BY opened_at DESC
         LIMIT ? OFFSET ?",
    )
    .bind(&wallet_address)
    .bind(&q.status)
    .bind(&q.status)
    .bind(&q.strategy_id)
    .bind(&q.strategy_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("list positions", e))?;

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM positions
            WHERE wallet_address = ?
              AND (? IS NULL OR status = ?)
              AND (? IS NULL OR strategy_id = ?)",
    )
    .bind(&wallet_address)
    .bind(&q.status)
    .bind(&q.status)
    .bind(&q.strategy_id)
    .bind(&q.strategy_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| db_error("count positions", e))?;

    let has_more = offset + (positions.len() as i64) < total;
    let mut headers = HeaderMap::new();
    headers.insert("x-total-count", HeaderValue::from(total));
    headers.insert(
        "x-has-more",
        HeaderValue::from_static(if has_more { "true" } else { "false" }),
    );

    Ok((headers, Json(positions)))
}

#[derive(Deserialize)]
pub struct OpenPositionRequest {
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String,   // "call" | "put"
    pub position_type: String, // "long" | "short"
    pub contracts: f64,
}

/// Loads the wallet's currently-open positions inside the caller's
/// transaction. The margin engine works off this post-trade-visible set so
/// the what-if check and the commit see exactly the same book.
pub(crate) async fn load_open_positions_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    wallet_address: &str,
) -> Result<Vec<Position>, AppError> {
    sqlx::query_as(
        "SELECT * FROM positions WHERE wallet_address = ? AND status = 'open'",
    )
    .bind(wallet_address)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| db_error("load open positions", e))
}

/// Computes the portfolio margin requirement for a wallet's post-trade
/// position set using the environment-selected model. Returns the initial
/// requirement, the maintenance requirement, the worst stress scenario and
/// the per-position contribution breakdown.
pub(crate) fn portfolio_requirement(
    positions: &[Position],
) -> crate::margin::MarginRequirement {
    margin_model().requirement(positions)
}

/// Prices and inserts a new position, debiting/crediting the account and
/// locking collateral as needed, all within the caller's transaction.
/// Shared by the open handler and (once it exists) the roll handler, so
/// rolling a position doesn't need to duplicate this logic.
pub(crate) async fn open_position_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    state: &AppState,
    wallet_address: &str,
    req: &OpenPositionRequest,
    strategy_id: Option<&str>,
) -> Result<Position, AppError> {
    if req.contracts <= 0.0 || req.strike <= 0.0 || req.expiry_days <= 0.0 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "contracts, strike, and expiry_days must all be positive",
        ));
    }
    if req.option_type != "call" && req.option_type != "put" {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "option_type must be \"call\" or \"put\"",
        ));
    }
    if req.position_type != "long" && req.position_type != "short" {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "position_type must be \"long\" or \"short\"",
        ));
    }

    let (spot, base_vol) = {
        let prices = state.spot_prices.lock().unwrap();
        let vols = state.vol_surface.lock().unwrap();
        let not_found = || {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("unknown underlying \"{}\"", req.underlying),
            )
        };
        let spot = *prices.get(&req.underlying).ok_or_else(not_found)?;
        let vol = *vols.get(&req.underlying).ok_or_else(not_found)?;
        (spot, vol)
    };

    let vol = smile_vol(base_vol, req.strike / spot);
    let t = req.expiry_days / 365.0;
    let is_call = req.option_type == "call";
    let entry_premium = black_scholes(&BSInputs {
        spot,
        strike: req.strike,
        vol,
        t,
        r: 0.05,
        is_call,
    })
    .premium;

    let is_short = req.position_type == "short";
    let cash_delta = if is_short {
        entry_premium * req.contracts // premium received
    } else {
        -entry_premium * req.contracts // premium paid
    };

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(wallet_address)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| db_error("load account", e))?;

    // Build the post-trade position set (existing open legs plus the leg
    // about to be inserted) and run the portfolio margin engine over it.
    // This is the what-if: it happens before any write, inside the same
    // transaction, so a concurrent open cannot slip past the check.
    let mut post_trade = load_open_positions_in_tx(tx, wallet_address).await?;
    post_trade.push(Position {
        id: String::new(),
        wallet_address: wallet_address.to_string(),
        underlying: req.underlying.clone(),
        strike: req.strike,
        expiry_days: req.expiry_days,
        option_type: req.option_type.clone(),
        position_type: req.position_type.clone(),
        contracts: req.contracts,
        entry_premium,
        entry_spot: spot,
        collateral: 0.0,
        status: "open".to_string(),
        strategy_id: strategy_id.map(|s| s.to_string()),
    });

    let requirement = portfolio_requirement(&post_trade);
    let new_balance = account.balance + cash_delta;
    let new_collateral_locked = requirement.initial;

    // A trade is rejected with 422 if the post-trade initial margin would
    // exceed equity (balance minus the portfolio requirement).
    if new_balance - new_collateral_locked < 0.0 {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "insufficient buying power: post-trade initial margin would exceed equity",
        ));
    }

    sqlx::query("UPDATE accounts SET balance = ?, collateral_locked = ? WHERE wallet_address = ?")
        .bind(new_balance)
        .bind(new_collateral_locked)
        .bind(wallet_address)
        .execute(&mut **tx)
        .await
        .map_err(|e| db_error("update account balance", e))?;

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO positions
            (id, wallet_address, underlying, strike, expiry_days, option_type,
             position_type, contracts, entry_premium, entry_spot, collateral, status, strategy_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?)",
    )
    .bind(&id)
    .bind(wallet_address)
    .bind(&req.underlying)
    .bind(req.strike)
    .bind(req.expiry_days)
    .bind(&req.option_type)
    .bind(&req.position_type)
    .bind(req.contracts)
    .bind(entry_premium)
    .bind(spot)
    .bind(requirement.contribution_for(&id))
    .bind(strategy_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_error("insert position", e))?;

    let position: Position = sqlx::query_as("SELECT * FROM positions WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| db_error("load position", e))?;

    Ok(position)
}

/// Closes an open position inside the caller's transaction, releasing its
/// share of collateral and recomputing the wallet's portfolio requirement
/// over the remaining book.
pub(crate) async fn close_position_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    wallet_address: &str,
    position_id: &str,
) -> Result<Position, AppError> {
    let position: Position = sqlx::query_as(
        "SELECT * FROM positions WHERE id = ? AND wallet_address = ? AND status = 'open'",
    )
    .bind(position_id)
    .bind(wallet_address)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| db_error("load position to close", e))?;

    sqlx::query("UPDATE positions SET status = 'closed' WHERE id = ?")
        .bind(position_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| db_error("close position", e))?;

    let remaining = load_open_positions_in_tx(tx, wallet_address).await?;
    let requirement = portfolio_requirement(&remaining);

    sqlx::query("UPDATE accounts SET collateral_locked = ? WHERE wallet_address = ?")
        .bind(requirement.initial)
        .bind(wallet_address)
        .execute(&mut **tx)
        .await
        .map_err(|e| db_error("update account collateral", e))?;

    Ok(position)
}

/// `GET /api/v1/account/margin` — returns the initial requirement, the
/// maintenance requirement, the worst stress scenario and a per-position
/// contribution breakdown for the authenticated wallet.
pub async fn get_margin(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<crate::margin::MarginRequirement>, AppError> {
    let positions: Vec<Position> = sqlx::query_as(
        "SELECT * FROM positions WHERE wallet_address = ? AND status = 'open'",
    )
    .bind(&wallet_address)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("load open positions", e))?;

    Ok(Json(portfolio_requirement(&positions)))
}
