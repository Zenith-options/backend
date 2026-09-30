use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Json;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{Sqlite, Transaction};

use crate::auth::AuthUser;
use crate::collateral::collateral_required;
use crate::error::{db_error, AppError, AppJson, AppQuery};
use crate::models::{Account, Position};
use crate::{black_scholes, smile_vol, AppState, BSInputs, BSResult};

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

#[derive(Deserialize)]
pub struct OpenPositionRequest {
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String,   // "call" | "put"
    pub position_type: String, // "long" | "short"
    pub contracts: f64,
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
    let collateral = if is_short {
        collateral_required(&req.option_type, req.contracts, req.strike, spot)
    } else {
        0.0
    };
    let cash_delta = if is_short {
        entry_premium * req.contracts // premium received
    } else {
        -entry_premium * req.contracts // premium paid
    };

    // Serialise on the account row so a reset can't interleave with an
    // in-flight fill: the reset takes the same row lock before closing
    // positions and starting a new epoch.
    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(wallet_address)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| db_error("load account", e))?;

    let epoch_id = ensure_current_epoch(tx, wallet_address).await?;

    let new_balance = account.balance + cash_delta;
    let new_collateral_locked = account.collateral_locked + collateral;
    // Available buying power must stay non-negative: cash on hand minus
    // whatever's locked as collateral (across all positions, not just
    // this one) must cover this trade's premium debit/collateral.
    if new_balance - new_collateral_locked < 0.0 {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "insufficient buying power: this trade's premium/collateral would exceed balance minus locked collateral",
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
             position_type, contracts, entry_premium, entry_spot, collateral, status, strategy_id, epoch_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?, ?)",
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
    .bind(collateral)
    .bind(strategy_id)
    .bind(epoch_id)
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
