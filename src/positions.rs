use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Json;
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, Transaction};

use crate::auth::AuthUser;
use crate::collateral::collateral_required;
use crate::error::{db_error, AppError, AppJson, AppQuery};
use crate::models::{Account, Position};
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

    // Snap the requested `expiry_days` to the nearest listed expiry from the
    // expiry calendar so every position carries a real, tradeable expiry.
    // Documented behaviour: we snap (rather than reject) to keep the existing
    // `expiry_days` API working while positions gain absolute `expires_at`.
    let expires_at = crate::expiry::snap_expiry(&state, &req.underlying, req.expiry_days)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("no listed expiry for \"{}\"", req.underlying),
            )
        })?;

    let t = time_to_expiry_years(&expires_at, crate::now_unix());
    let sigma = smile_vol(base_vol, req.strike, spot, t);
    let bs = black_scholes(BSInputs {
        spot,
        strike: req.strike,
        t,
        vol: sigma,
        option_type: req.option_type.clone(),
    });

    let premium = bs.price;
    let notional = premium * req.contracts;
    let collateral = collateral_required(
        &req.position_type,
        &req.option_type,
        spot,
        req.strike,
        req.contracts,
    );

    // Long positions pay the premium up front; short positions receive it
    // but must lock collateral. Both effects land on the same balance.
    let balance_delta = if req.position_type == "long" {
        -notional
    } else {
        notional - collateral
    };

    let account: Account = sqlx::query_as("SELECT * FROM accounts WHERE wallet_address = ?")
        .bind(wallet_address)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| db_error("load account for open", e))?;

    if account.balance + balance_delta < 0.0 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "insufficient balance to open position",
        ));
    }

    sqlx::query(
        "UPDATE accounts
            SET balance = balance + ?,
                locked_collateral = locked_collateral + ?
          WHERE wallet_address = ?",
    )
    .bind(balance_delta)
    .bind(collateral)
    .bind(wallet_address)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_error("update account for open", e))?;

    let position: Position = sqlx::query_as(
        "INSERT INTO positions
            (wallet_address, underlying, strike, expiry_days, expires_at, option_type,
             position_type, contracts, open_premium, open_spot, collateral, status, strategy_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?)
         RETURNING *",
    )
    .bind(wallet_address)
    .bind(&req.underlying)
    .bind(req.strike)
    .bind(req.expiry_days)
    .bind(&expires_at)
    .bind(&req.option_type)
    .bind(&req.position_type)
    .bind(req.contracts)
    .bind(premium)
    .bind(spot)
    .bind(collateral)
    .bind(strategy_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| db_error("insert position", e))?;

    Ok(position)
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

    let intrinsic = intrinsic_value(&position.option_type, position.strike, fixing);
    let notional = intrinsic * position.contracts;

    // Long: the holder receives intrinsic value (premium was paid at open).
    // Short: the writer pays intrinsic value out of the locked collateral;
    // if collateral is short of the liability we settle what is available
    // and leave the account balance at zero rather than going negative.
    let (balance_delta, collateral_release) = if position.position_type == "long" {
        (notional, position.collateral)
    } else {
        let liability = notional.min(position.collateral);
        (-liability, position.collateral)
    };

    let realized_pnl = if position.position_type == "long" {
        notional - position.open_premium * position.contracts
    } else {
        position.open_premium * position.contracts - notional
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
