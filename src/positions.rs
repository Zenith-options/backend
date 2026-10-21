use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Json;
use base64::Engine;
use hmac::{Hmac, Mac};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{Sqlite, Transaction};
use std::collections::HashMap;

use crate::auth::AuthUser;
use crate::collateral::collateral_required;
use crate::error::{db_error, AppError, AppJson, AppQuery};
use crate::margin::{MarginLeg, MarginModel, MarginRequirement, RiskArrayMargin, Scenario, StrategyBasedMargin};
use crate::models::{Account, Money, Position, Qty};
use crate::{black_scholes, smile_vol, AppState, BSInputs, BSResult};

/// Seconds in a (Julian) year, used to convert an absolute time-to-expiry
/// into the `t` Black-Scholes expects.
const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 60.0 * 60.0;

/// Derives the Black-Scholes time-to-expiry `t` (in years) from a position's
/// absolute `expires_at` and the current time. Clamped at zero so a position
/// held past expiry reprices at intrinsic value instead of producing a
/// negative `t` (which would yield NaN in Black-Scholes).
pub(crate) fn time_to_expiry_years(expires_at: &str, now_unix: i64) -> f64 {
    let expiry_unix = parse_iso8601_utc(expires_at).unwrap_or(now_unix);
    let remaining = (expiry_unix - now_unix).max(0) as f64;
    remaining / SECONDS_PER_YEAR
}

/// Minimal ISO-8601 UTC parser (`YYYY-MM-DDTHH:MM:SSZ`), returning Unix
/// seconds. Kept local so this module doesn't pull in a new dependency; the
/// migration writes exactly this format.
fn parse_iso8601_utc(s: &str) -> Option<i64> {
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next().unwrap_or("0").parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since the Unix epoch for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil` algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Intrinsic value of an option at settlement: the payoff a holder would
/// realise if exercised immediately at the fixing price. Calls are worth
/// `max(spot - strike, 0)`, puts `max(strike - spot, 0)`. Shared with the
/// settlement engine so manual closes and automated expiry agree exactly.
pub(crate) fn intrinsic_value(option_type: &str, strike: f64, spot: f64) -> f64 {
    if option_type == "call" {
        (spot - strike).max(0.0)
    } else {
        (strike - spot).max(0.0)
    }
}

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

fn position_to_margin_leg(position: &Position) -> MarginLeg {
    MarginLeg {
        underlying: position.underlying.clone(),
        is_call: position.option_type == "call",
        quantity: position.contracts.as_decimal(),
        strike: position.strike.as_decimal(),
        spot: position.entry_spot.as_decimal(),
        iv: Decimal::ZERO,
        time_to_expiry: Decimal::ZERO,
    }
}

/// Default starting balance for a fresh paper account / epoch.
pub const DEFAULT_STARTING_BALANCE: f64 = 100_000.0;

/// Returns the wallet's current (open) epoch id, creating the account and
/// its first epoch lazily if they don't exist yet. This keeps the old
/// "lazily created account" behaviour while giving every account an
/// explicit epoch to scope history, stats and leaderboards against.
pub(crate) async fn ensure_current_epoch(
    tx: &mut Transaction<'_, Sqlite>,
    wallet_address: &str,
) -> Result<i64, AppError> {
    sqlx::query(
        "INSERT INTO accounts (wallet_address) VALUES (?) ON CONFLICT(wallet_address) DO NOTHING",
    )
    .bind(wallet_address)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_error("create or confirm account", e))?;

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT current_epoch_id FROM accounts WHERE wallet_address = ?",
    )
    .bind(wallet_address)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| db_error("load account epoch", e))?;

    if let Some(epoch_id) = existing {
        return Ok(epoch_id);
    }

    let epoch_id: i64 = sqlx::query_scalar(
        "INSERT INTO account_epochs (wallet_address, starting_balance)
         VALUES (?, ?) RETURNING id",
    )
    .bind(wallet_address)
    .bind(DEFAULT_STARTING_BALANCE)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| db_error("create account epoch", e))?;

    sqlx::query("UPDATE accounts SET current_epoch_id = ? WHERE wallet_address = ?")
        .bind(epoch_id)
        .bind(wallet_address)
        .execute(&mut **tx)
        .await
        .map_err(|e| db_error("set current epoch", e))?;

    Ok(epoch_id)
}

pub async fn get_account(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Account>, AppError> {
    // Verify/login already creates this row, but stay defensive in case a
    // session outlives some future account-deletion path. We also make
    // sure the account has a current epoch so history/stats scoping works.
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| db_error("begin account tx", e))?;
    ensure_current_epoch(&mut tx, &wallet_address).await?;
    tx.commit()
        .await
        .map_err(|e| db_error("commit account tx", e))?;

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(&wallet_address)
        .fetch_one(&state.db)
        .await
        .map_err(|e| db_error("load account", e))?;

    Ok(Json(account))
}

pub const DEFAULT_LIST_LIMIT: i64 = 50;
pub const MAX_LIST_LIMIT: i64 = 200;

/// Opaque, HMAC-signed keyset cursor. Encodes the `(sort_key, id)` pair
/// that the next page should resume after, plus a fingerprint of the
/// filter set it was minted under so a cursor can't be replayed against a
/// different query. base64url-encoded so it's safe in a query string.
#[derive(Serialize, Deserialize)]
struct CursorPayload {
    /// `opened_at` of the last row on the previous page.
    sort_key: String,
    /// Tiebreaker id of the last row on the previous page.
    id: String,
    /// Fingerprint of the filters this cursor was issued for.
    filters: String,
}

fn cursor_secret(state: &AppState) -> &[u8] {
    state.cursor_secret.as_bytes()
}

fn filters_fingerprint(status: Option<&str>, strategy_id: Option<&str>) -> String {
    format!(
        "status={};strategy_id={}",
        status.unwrap_or(""),
        strategy_id.unwrap_or("")
    )
}

fn encode_cursor(state: &AppState, payload: &CursorPayload) -> Result<String, AppError> {
    let json = serde_json::to_vec(payload)
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(cursor_secret(state))
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    mac.update(&json);
    let sig = mac.finalize().into_bytes();
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Ok(format!(
        "{}.{}",
        engine.encode(&json),
        engine.encode(sig)
    ))
}

fn decode_cursor(state: &AppState, token: &str) -> Result<CursorPayload, AppError> {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (body, sig) = token.split_once('.').ok_or_else(|| {
        AppError::new(StatusCode::BAD_REQUEST, "malformed cursor")
    })?;
    let json = engine
        .decode(body)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "malformed cursor"))?;
    let sig = engine
        .decode(sig)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "malformed cursor"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(cursor_secret(state))
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    mac.update(&json);
    mac.verify_slice(&sig)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "invalid cursor signature"))?;
    serde_json::from_slice(&json)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "malformed cursor"))
}

#[derive(Deserialize)]
pub struct ListPositionsQuery {
    /// "open" | "closed" | "rolled" — omit to return every status.
    pub status: Option<String>,
    /// Restrict to the legs of one multi-leg strategy — omit for everything.
    pub strategy_id: Option<String>,
    /// Defaults to DEFAULT_LIST_LIMIT, capped at MAX_LIST_LIMIT regardless
    /// of what the caller asks for.
    pub limit: Option<i64>,
    /// Deprecated offset fallback, kept for one release.
    pub offset: Option<i64>,
    /// Opaque keyset cursor from a previous page's `next_cursor`.
    pub cursor: Option<String>,
    /// "current" (default) scopes to the account's current epoch;
    /// "all" includes every prior epoch's history.
    pub epoch: Option<String>,
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

    // Keyset path: resume strictly after the `(opened_at, id)` pair the
    // cursor encodes. Fetch one extra row to know whether a next page
    // exists without a COUNT.
    if let Some(token) = q.cursor.as_deref() {
        let payload = decode_cursor(&state, token)?;
        let expected = filters_fingerprint(q.status.as_deref(), q.strategy_id.as_deref());
        if payload.filters != expected {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "cursor does not match the current filters",
            ));
        }

        let mut rows: Vec<Position> = sqlx::query_as(
            "SELECT * FROM positions
                WHERE wallet_address = ?
                  AND (? IS NULL OR status = ?)
                  AND (? IS NULL OR strategy_id = ?)
                  AND (opened_at < ? OR (opened_at = ? AND id < ?))
             ORDER BY opened_at DESC, id DESC
             LIMIT ?",
        )
        .bind(&wallet_address)
        .bind(&q.status)
        .bind(&q.status)
        .bind(&q.strategy_id)
        .bind(&q.strategy_id)
        .bind(&payload.sort_key)
        .bind(&payload.sort_key)
        .bind(&payload.id)
        .bind(limit + 1)
        .fetch_all(&state.db)
        .await
        .map_err(|e| db_error("list positions", e))?;

        let has_more = rows.len() as i64 > limit;
        if has_more {
            rows.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            rows.last().map(|p| {
                encode_cursor(
                    &state,
                    &CursorPayload {
                        sort_key: p.opened_at.clone(),
                        id: p.id.clone(),
                        filters: expected,
                    },
                )
            })
            .transpose()?
        } else {
            None
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-has-more",
            HeaderValue::from_static(if has_more { "true" } else { "false" }),
        );
        if let Some(c) = next_cursor {
            if let Ok(v) = HeaderValue::from_str(&c) {
                headers.insert("x-next-cursor", v);
            }
        }

        return Ok((headers, Json(rows)));
    }

    // Deprecated offset fallback (kept for one release).
    let offset = q.offset.unwrap_or(0).max(0);

    // `?epoch=all` includes every epoch; anything else (including the
    // default) scopes to the account's current epoch. A NULL epoch_id
    // (legacy rows) is treated as belonging to the current epoch so old
    // data isn't silently hidden.
    let include_all = matches!(q.epoch.as_deref(), Some("all"));
    let current_epoch: Option<i64> = sqlx::query_scalar(
        "SELECT current_epoch_id FROM accounts WHERE wallet_address = ?",
    )
    .bind(&wallet_address)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| db_error("load current epoch", e))?
    .flatten();

    // `? IS NULL OR column = ?` lets one query handle all four
    // status/strategy_id filter combinations without branching SQL.
    let positions: Vec<Position> = sqlx::query_as(
        "SELECT * FROM positions
            WHERE wallet_address = ?
              AND (? IS NULL OR status = ?)
              AND (? IS NULL OR strategy_id = ?)
              AND (? = 1 OR epoch_id IS NULL OR epoch_id = ?)
         ORDER BY opened_at DESC, id DESC
         LIMIT ? OFFSET ?",
    )
    .bind(&wallet_address)
    .bind(&q.status)
    .bind(&q.status)
    .bind(&q.strategy_id)
    .bind(&q.strategy_id)
    .bind(include_all)
    .bind(current_epoch)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("list positions", e))?;

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM positions
            WHERE wallet_address = ?
              AND (? IS NULL OR status = ?)
              AND (? IS NULL OR strategy_id = ?)
              AND (? = 1 OR epoch_id IS NULL OR epoch_id = ?)",
    )
    .bind(&wallet_address)
    .bind(&q.status)
    .bind(&q.status)
    .bind(&q.strategy_id)
    .bind(&q.strategy_id)
    .bind(include_all)
    .bind(current_epoch)
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

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct OpenPositionRequest {
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String,   // "call" | "put"
    pub position_type: String, // "long" | "short"
    pub contracts: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AggregateGreeks {
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
    pub rho: f64,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RollPositionRequest {
    pub new_position: OpenPositionRequest,
}

#[derive(Serialize, Clone, Debug)]
pub struct RollResult {
    pub closed: Position,
    pub opened: Position,
}

pub async fn open_position(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    AppJson(req): AppJson<OpenPositionRequest>,
) -> Result<Json<Position>, AppError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| db_error("begin open position tx", e))?;
    let position = open_position_in_tx(&mut tx, &state, &wallet_address, &req, None).await?;
    tx.commit()
        .await
        .map_err(|e| db_error("commit open position tx", e))?;
    Ok(Json(position))
}

pub async fn get_portfolio_greeks(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<AggregateGreeks>, AppError> {
    let positions: Vec<Position> = sqlx::query_as(
        "SELECT * FROM positions WHERE wallet_address = ? AND status = 'open'",
    )
    .bind(&wallet_address)
    .fetch_all(&state.db)
    .await
    .map_err(|e| db_error("load portfolio greeks", e))?;

    let mut greeks = AggregateGreeks::default();
    for position in &positions {
        let Some(result) = current_bs_result(&state, position) else { continue };
        let qty = position.contracts.to_f64();
        greeks.delta += result.delta * qty;
        greeks.gamma += result.gamma * qty;
        greeks.vega += result.vega * qty;
        greeks.theta += result.theta * qty;
        greeks.rho += result.rho * qty;
    }

    Ok(Json(greeks))
}

pub async fn close_position(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Path(position_id): Path<String>,
) -> Result<Json<Position>, AppError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| db_error("begin close position tx", e))?;
    let position = close_position_in_tx(&mut tx, &wallet_address, &position_id).await?;
    tx.commit()
        .await
        .map_err(|e| db_error("commit close position tx", e))?;
    Ok(Json(position))
}

pub async fn roll_position(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Path(position_id): Path<String>,
    AppJson(req): AppJson<OpenPositionRequest>,
) -> Result<Json<Position>, AppError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| db_error("begin roll position tx", e))?;
    let _ = close_position_in_tx(&mut tx, &wallet_address, &position_id).await?;
    let position = open_position_in_tx(&mut tx, &state, &wallet_address, &req, None).await?;
    tx.commit()
        .await
        .map_err(|e| db_error("commit roll position tx", e))?;
    Ok(Json(position))
}

pub fn current_bs_result(state: &AppState, position: &Position) -> Option<BSResult> {
    let prices = state.spot_prices.lock().unwrap();
    let vols = state.vol_surface.lock().unwrap();
    let spot = *prices.get(&position.underlying)?;
    let base_vol = *vols.get(&position.underlying)?;
    let t = time_to_expiry_years(&position.expires_at, crate::now_unix());
    let sigma = smile_vol(base_vol, position.strike.to_f64() / spot.max(f64::EPSILON));
    Some(black_scholes(&BSInputs {
        spot,
        strike: position.strike.to_f64(),
        t,
        vol: sigma,
        r: 0.0,
        is_call: position.option_type == "call",
    }))
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
pub(crate) fn portfolio_requirement(positions: &[Position]) -> MarginRequirement {
    let legs: Vec<MarginLeg> = positions.iter().map(position_to_margin_leg).collect();
    match margin_model().requirement(&legs) {
        Ok(req) => req,
        Err(_) => MarginRequirement {
            initial: Decimal::ZERO,
            maintenance: Decimal::ZERO,
            worst_scenario: Scenario {
                spot_shock: Decimal::ZERO,
                vol_shock: Decimal::ZERO,
            },
            contributions: HashMap::new(),
        },
    }
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

    let expires_at = chrono::DateTime::from_timestamp(
        crate::now_unix() + (req.expiry_days as i64).max(1) * 86_400,
        0,
    )
    .unwrap_or_else(|| chrono::Utc::now())
    .to_rfc3339();

    let t = time_to_expiry_years(&expires_at, crate::now_unix());
    let sigma = smile_vol(base_vol, req.strike / spot.max(f64::EPSILON));
    let bs = black_scholes(&BSInputs {
        spot,
        strike: req.strike,
        t,
        vol: sigma,
        r: 0.0,
        is_call: req.option_type == "call",
    });

    let premium = bs.premium;
    let is_short = req.position_type == "short";
    let collateral = if is_short {
        collateral_required(&req.option_type, req.contracts, req.strike, spot)
            .map(|m| m.value().to_f64().unwrap_or(0.0))
            .unwrap_or(0.0)
    } else {
        0.0
    };
    let balance_delta = if is_short {
        premium * req.contracts - collateral
    } else {
        -(premium * req.contracts)
    };

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(wallet_address)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| db_error("load account for open", e))?;

    let epoch_id = ensure_current_epoch(tx, wallet_address).await?;

    let mut post_trade = load_open_positions_in_tx(tx, wallet_address).await?;
    let position_id = uuid::Uuid::new_v4().to_string();
    post_trade.push(Position {
        id: position_id.clone(),
        wallet_address: wallet_address.to_string(),
        underlying: req.underlying.clone(),
        strike: Money::from_price(req.strike).unwrap_or(Money::ZERO),
        expiry_days: req.expiry_days,
        expires_at: expires_at.clone(),
        option_type: req.option_type.clone(),
        position_type: req.position_type.clone(),
        contracts: Qty::from_price(req.contracts).unwrap_or(Qty::ZERO),
        entry_premium: Money::from_price(premium).unwrap_or(Money::ZERO),
        entry_spot: Money::from_price(spot).unwrap_or(Money::ZERO),
        collateral: Money::from_price(collateral).unwrap_or(Money::ZERO),
        status: "open".to_string(),
        close_premium: None,
        close_spot: None,
        realized_pnl: None,
        opened_at: crate::now_unix().to_string(),
        closed_at: None,
        strategy_id: strategy_id.map(|s| s.to_string()),
    });

    let requirement = portfolio_requirement(&post_trade);
    let new_balance = account.balance.to_f64() + balance_delta;
    let new_collateral_locked = requirement.initial.to_f64().unwrap_or(0.0);

    if new_balance - new_collateral_locked < 0.0 {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "insufficient buying power: post-trade initial margin would exceed equity",
        ));
    }

    let collateral_money = Money::from_price(collateral).unwrap_or(Money::ZERO);
    let delta_money = Money::from_price(balance_delta).unwrap_or(Money::ZERO);
    sqlx::query(
        "UPDATE accounts
            SET balance = balance + ?,
                collateral_locked = collateral_locked + ?
          WHERE wallet_address = ?",
    )
    .bind(delta_money)
    .bind(collateral_money)
    .bind(wallet_address)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_error("update account for open", e))?;

    let inserted: Position = sqlx::query_as(
        "INSERT INTO positions
            (id, wallet_address, underlying, strike, expiry_days, expires_at, option_type,
             position_type, contracts, entry_premium, entry_spot, collateral, status, opened_at,
             strategy_id, epoch_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?, ?, ?)
         RETURNING *",
    )
    .bind(&position_id)
    .bind(wallet_address)
    .bind(&req.underlying)
    .bind(Money::from_price(req.strike).unwrap_or(Money::ZERO))
    .bind(req.expiry_days)
    .bind(&expires_at)
    .bind(&req.option_type)
    .bind(&req.position_type)
    .bind(Qty::from_price(req.contracts).unwrap_or(Qty::ZERO))
    .bind(Money::from_price(premium).unwrap_or(Money::ZERO))
    .bind(Money::from_price(spot).unwrap_or(Money::ZERO))
    .bind(collateral_money)
    .bind(crate::now_unix().to_string())
    .bind(strategy_id)
    .bind(epoch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| db_error("insert position", e))?;

    Ok(inserted)
}

/// Settles a single position at expiry inside the caller's transaction.
///
/// This is the shared settlement math used by both the manual close handler
/// and the automated expiry engine: the position is marked `expired`, its
/// `close_premium` is the intrinsic value at the fixing price, `close_spot`
/// is the fixing itself, and `realized_pnl` is the net cash effect for the
/// holder. Collateral is released back to the account.
///
/// Returns the realised P&L so callers (e.g. the settlement engine) can
/// aggregate it. Idempotency is the caller's responsibility: only rows still
/// in `status = 'open'` should be passed in, and the `WHERE status = 'open'`
/// guard below makes a double-settle a no-op that returns `None`.
pub(crate) async fn settle_position_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    position_id: i64,
    fixing: f64,
) -> Result<Option<f64>, AppError> {
    let position: Position = sqlx::query_as("SELECT * FROM positions WHERE id = ?")
        .bind(position_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| db_error("load position for settlement", e))?;

    // Guard against double-settlement: if the row already left `open`
    // (manual close, prior settlement run, roll), do nothing.
    if position.status != "open" {
        return Ok(None);
    }

    let intrinsic = intrinsic_value(&position.option_type, position.strike.to_f64(), fixing);
    let notional = intrinsic * position.contracts.to_f64();

    // Long: the holder receives intrinsic value (premium was paid at open).
    // Short: the writer pays intrinsic value out of the locked collateral;
    // if collateral is short of the liability we settle what is available
    // and leave the account balance at zero rather than going negative.
    let collateral = position.collateral.to_f64();
    let entry_premium = position.entry_premium.to_f64();
    let (balance_delta, collateral_release) = if position.position_type == "long" {
        (notional, collateral)
    } else {
        let liability = notional.min(collateral);
        (-liability, collateral)
    };

    let realized_pnl = if position.position_type == "long" {
        notional - entry_premium * position.contracts.to_f64()
    } else {
        entry_premium * position.contracts.to_f64() - notional
    };

    sqlx::query(
        "UPDATE accounts
            SET balance = balance + ?,
                locked_collateral = locked_collateral - ?
          WHERE wallet_address = ?",
    )
    .bind(balance_delta)
    .bind(collateral_release)
    .bind(&position.wallet_address)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_error("release collateral on settlement", e))?;

    sqlx::query(
        "UPDATE positions
            SET status = 'expired',
                close_premium = ?,
                close_spot = ?,
                realized_pnl = ?,
                closed_at = ?
          WHERE id = ? AND status = 'open'",
    )
    .bind(intrinsic)
    .bind(fixing)
    .bind(realized_pnl)
    .bind(crate::now_unix())
    .bind(position_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_error("mark position expired", e))?;

    Ok(Some(realized_pnl))
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
        .bind(requirement.initial.to_f64())
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

