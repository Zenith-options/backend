//! Wallet-scoped RFQ negotiation records.
//!
//! Acceptance records an off-chain agreement only. It does not transfer
//! collateral, premiums, or positions; those require the Soroban contracts
//! and transaction pipeline described in `docs/stellar-settlement.md`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashSet;
use uuid::Uuid;

use crate::AppState;

const MAX_COUNTERPARTIES: usize = 20;
const MAX_SERIES_LENGTH: usize = 128;
const MAX_RFQ_TTL_SECS: i64 = 3_600;

#[derive(Debug, Serialize)]
pub struct ApiError {
    error: &'static str,
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(ApiError { error: message })).into_response()
}

async fn authenticated_wallet(
    headers: &HeaderMap,
    pool: &SqlitePool,
) -> Result<String, Response> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "authentication required"))?;

    sqlx::query_scalar::<_, String>(
        "SELECT wallet_address FROM sessions
         WHERE token = ?1 AND julianday(expires_at) > julianday('now')",
    )
    .bind(token)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "failed to look up RFQ session");
        error(StatusCode::INTERNAL_SERVER_ERROR, "authentication unavailable")
    })?
    .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "invalid or expired session"))
}

#[derive(Debug, Deserialize)]
pub struct CreateRfqRequest {
    pub series: String,
    pub side: String,
    pub contracts: i64,
    pub counterparties: Vec<String>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SubmitQuoteRequest {
    /// Premium in micro-USDC; integer units avoid floating-point settlement
    /// arithmetic in the negotiation record.
    pub premium_microusd: i64,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct AcceptQuoteRequest {
    pub quote_id: String,
}

#[derive(Debug, Serialize)]
pub struct RfqQuote {
    pub id: String,
    pub maker_wallet: String,
    pub premium_microusd: i64,
    pub status: String,
    pub expires_at: i64,
}

#[derive(Debug, Serialize)]
pub struct RfqResponse {
    pub id: String,
    pub requester_wallet: String,
    pub series: String,
    pub side: String,
    pub contracts: i64,
    pub counterparties: Vec<String>,
    pub status: String,
    pub expires_at: i64,
    pub accepted_quote_id: Option<String>,
    pub quotes: Vec<RfqQuote>,
    /// Acceptance is not on-chain delivery-versus-payment.
    pub settlement: &'static str,
}

fn valid_wallet(address: &str) -> bool {
    crate::strkey::decode_stellar_public_key(address).is_ok()
}

fn valid_ttl(ttl: Option<i64>) -> Option<i64> {
    let ttl = ttl.unwrap_or(300);
    (30..=MAX_RFQ_TTL_SECS).contains(&ttl).then_some(ttl)
}

pub async fn create_rfq(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateRfqRequest>,
) -> Response {
    let requester = match authenticated_wallet(&headers, &state.db).await {
        Ok(wallet) => wallet,
        Err(response) => return response,
    };

    if request.series.trim().is_empty()
        || request.series.len() > MAX_SERIES_LENGTH
        || !request.series.is_ascii()
        || request.series.chars().any(char::is_control)
        || !matches!(request.side.as_str(), "buy" | "sell")
        || request.contracts <= 0
        || request.counterparties.is_empty()
        || request.counterparties.len() > MAX_COUNTERPARTIES
    {
        return error(StatusCode::BAD_REQUEST, "invalid RFQ terms");
    }
    let ttl = match valid_ttl(request.ttl_secs) {
        Some(ttl) => ttl,
        None => return error(StatusCode::BAD_REQUEST, "ttl_secs must be between 30 and 3600"),
    };

    let mut unique_counterparties = HashSet::new();
    if request.counterparties.iter().any(|wallet| {
        wallet == &requester
            || !valid_wallet(wallet)
            || !unique_counterparties.insert(wallet)
    }) {
        return error(
            StatusCode::BAD_REQUEST,
            "counterparties must be unique Stellar wallets other than the requester",
        );
    }

    let now = crate::quotes::now_secs() as i64;
    let id = Uuid::new_v4().to_string();
    let counterparties = match serde_json::to_string(&request.counterparties) {
        Ok(value) => value,
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize RFQ counterparties");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not create RFQ");
        }
    };
    let result = sqlx::query(
        "INSERT INTO rfqs
            (id, requester_wallet, series, side, contracts, counterparties,
             status, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8)",
    )
    .bind(&id)
    .bind(&requester)
    .bind(request.series.trim())
    .bind(&request.side)
    .bind(request.contracts)
    .bind(counterparties)
    .bind(now)
    .bind(now + ttl)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => load_rfq(&state.db, &id, &requester).await,
        Err(e) => {
            tracing::error!(error = %e, "failed to persist RFQ");
            error(StatusCode::INTERNAL_SERVER_ERROR, "could not create RFQ")
        }
    }
}

pub async fn submit_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(rfq_id): Path<String>,
    Json(request): Json<SubmitQuoteRequest>,
) -> Response {
    let maker = match authenticated_wallet(&headers, &state.db).await {
        Ok(wallet) => wallet,
        Err(response) => return response,
    };
    if request.premium_microusd <= 0 {
        return error(StatusCode::BAD_REQUEST, "premium_microusd must be positive");
    }
    let ttl = match valid_ttl(request.ttl_secs) {
        Some(ttl) => ttl,
        None => return error(StatusCode::BAD_REQUEST, "ttl_secs must be between 30 and 3600"),
    };

    let now = crate::quotes::now_secs() as i64;
    let id = Uuid::new_v4().to_string();
    let result = sqlx::query(
        "INSERT INTO rfq_quotes
            (id, rfq_id, maker_wallet, premium_microusd, status, created_at, expires_at)
         SELECT ?1, rfqs.id, ?2, ?3, 'offered', ?4,
                MIN(rfqs.expires_at, ?4 + ?5)
         FROM rfqs
         WHERE rfqs.id = ?6
           AND rfqs.status = 'open'
           AND rfqs.expires_at > ?4
           AND rfqs.requester_wallet <> ?2
           AND EXISTS (
               SELECT 1 FROM json_each(rfqs.counterparties)
               WHERE json_each.value = ?2
           )
         ON CONFLICT (rfq_id, maker_wallet) DO UPDATE SET
             premium_microusd = excluded.premium_microusd,
             status = 'offered',
             created_at = excluded.created_at,
             expires_at = excluded.expires_at",
    )
    .bind(&id)
    .bind(&maker)
    .bind(request.premium_microusd)
    .bind(now)
    .bind(ttl)
    .bind(&rfq_id)
    .execute(&state.db)
    .await;

    match result {
        Ok(result) if result.rows_affected() == 1 => {
            load_rfq(&state.db, &rfq_id, &maker).await
        }
        Ok(_) => error(
            StatusCode::NOT_FOUND,
            "RFQ is unavailable or this wallet is not an invited counterparty",
        ),
        Err(e) => {
            tracing::error!(error = %e, "failed to persist RFQ quote");
            error(StatusCode::INTERNAL_SERVER_ERROR, "could not submit quote")
        }
    }
}

pub async fn accept_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(rfq_id): Path<String>,
    Json(request): Json<AcceptQuoteRequest>,
) -> Response {
    let requester = match authenticated_wallet(&headers, &state.db).await {
        Ok(wallet) => wallet,
        Err(response) => return response,
    };
    let now = crate::quotes::now_secs() as i64;
    let mut transaction = match state.db.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(error = %e, "failed to begin RFQ acceptance transaction");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not accept quote");
        }
    };

    let accepted = sqlx::query(
        "UPDATE rfq_quotes
         SET status = 'accepted'
         WHERE id = ?1 AND rfq_id = ?2 AND status = 'offered' AND expires_at > ?3
           AND EXISTS (
               SELECT 1 FROM rfqs
               WHERE rfqs.id = ?2 AND rfqs.requester_wallet = ?4
                 AND rfqs.status = 'open' AND rfqs.expires_at > ?3
           )",
    )
    .bind(&request.quote_id)
    .bind(&rfq_id)
    .bind(now)
    .bind(&requester)
    .execute(&mut *transaction)
    .await;
    match accepted {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => {
            return error(
                StatusCode::CONFLICT,
                "quote is unavailable, expired, or RFQ is not open",
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to accept RFQ quote");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not accept quote");
        }
    }

    let updated = sqlx::query(
        "UPDATE rfqs SET status = 'accepted', accepted_quote_id = ?1
         WHERE id = ?2 AND requester_wallet = ?3 AND status = 'open' AND expires_at > ?4",
    )
    .bind(&request.quote_id)
    .bind(&rfq_id)
    .bind(&requester)
    .bind(now)
    .execute(&mut *transaction)
    .await;
    match updated {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => {
            return error(StatusCode::CONFLICT, "RFQ is no longer open");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to update RFQ acceptance");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not accept quote");
        }
    }

    if let Err(e) = sqlx::query(
        "UPDATE rfq_quotes SET status = 'withdrawn'
         WHERE rfq_id = ?1 AND id <> ?2 AND status = 'offered'",
    )
    .bind(&rfq_id)
    .bind(&request.quote_id)
    .execute(&mut *transaction)
    .await
    {
        tracing::error!(error = %e, "failed to withdraw competing RFQ quotes");
        return error(StatusCode::INTERNAL_SERVER_ERROR, "could not accept quote");
    }

    if let Err(e) = transaction.commit().await {
        tracing::error!(error = %e, "failed to commit RFQ acceptance");
        return error(StatusCode::INTERNAL_SERVER_ERROR, "could not accept quote");
    }
    load_rfq(&state.db, &rfq_id, &requester).await
}

pub async fn cancel_rfq(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(rfq_id): Path<String>,
) -> Response {
    let requester = match authenticated_wallet(&headers, &state.db).await {
        Ok(wallet) => wallet,
        Err(response) => return response,
    };
    let mut transaction = match state.db.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(error = %e, "failed to begin RFQ cancellation transaction");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not cancel RFQ");
        }
    };
    let result = sqlx::query(
        "UPDATE rfqs SET status = 'cancelled'
         WHERE id = ?1 AND requester_wallet = ?2 AND status = 'open'",
    )
    .bind(&rfq_id)
    .bind(&requester)
    .execute(&mut *transaction)
    .await;

    match result {
        Ok(result) if result.rows_affected() == 1 => {
            if let Err(e) = sqlx::query(
                "UPDATE rfq_quotes SET status = 'withdrawn'
                 WHERE rfq_id = ?1 AND status = 'offered'",
            )
            .bind(&rfq_id)
            .execute(&mut *transaction)
            .await
            {
                tracing::error!(error = %e, "failed to withdraw quotes for cancelled RFQ");
                return error(StatusCode::INTERNAL_SERVER_ERROR, "could not cancel RFQ");
            }
            if let Err(e) = transaction.commit().await {
                tracing::error!(error = %e, "failed to commit RFQ cancellation");
                return error(StatusCode::INTERNAL_SERVER_ERROR, "could not cancel RFQ");
            }
            load_rfq(&state.db, &rfq_id, &requester).await
        }
        Ok(_) => error(StatusCode::CONFLICT, "RFQ is not open or does not exist"),
        Err(e) => {
            tracing::error!(error = %e, "failed to cancel RFQ");
            error(StatusCode::INTERNAL_SERVER_ERROR, "could not cancel RFQ")
        }
    }
}

pub async fn get_rfq(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(rfq_id): Path<String>,
) -> Response {
    let wallet = match authenticated_wallet(&headers, &state.db).await {
        Ok(wallet) => wallet,
        Err(response) => return response,
    };
    load_rfq(&state.db, &rfq_id, &wallet).await
}

#[derive(sqlx::FromRow)]
struct RfqRow {
    id: String,
    requester_wallet: String,
    series: String,
    side: String,
    contracts: i64,
    counterparties: String,
    status: String,
    expires_at: i64,
    accepted_quote_id: Option<String>,
}

#[derive(sqlx::FromRow)]
struct QuoteRow {
    id: String,
    maker_wallet: String,
    premium_microusd: i64,
    status: String,
    expires_at: i64,
}

async fn load_rfq(pool: &SqlitePool, id: &str, wallet: &str) -> Response {
    let row = sqlx::query_as::<_, RfqRow>(
        "SELECT id, requester_wallet, series, side, contracts, counterparties,
                status, expires_at, accepted_quote_id
         FROM rfqs
         WHERE id = ?1
           AND (requester_wallet = ?2 OR EXISTS (
               SELECT 1 FROM json_each(counterparties) WHERE json_each.value = ?2
           ))",
    )
    .bind(id)
    .bind(wallet)
    .fetch_optional(pool)
    .await;

    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return error(StatusCode::NOT_FOUND, "RFQ not found"),
        Err(e) => {
            tracing::error!(error = %e, "failed to load RFQ");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not load RFQ");
        }
    };
    let quotes = sqlx::query_as::<_, QuoteRow>(
        "SELECT id, maker_wallet, premium_microusd, status, expires_at
         FROM rfq_quotes
         WHERE rfq_id = ?1
           AND (?2 = ?3 OR maker_wallet = ?2)
         ORDER BY created_at ASC",
    )
    .bind(&row.id)
    .bind(wallet)
    .bind(&row.requester_wallet)
    .fetch_all(pool)
    .await;
    let quotes = match quotes {
        Ok(rows) => rows
            .into_iter()
            .map(|quote| RfqQuote {
                id: quote.id,
                maker_wallet: quote.maker_wallet,
                premium_microusd: quote.premium_microusd,
                status: if quote.status == "offered"
                    && quote.expires_at <= crate::quotes::now_secs() as i64
                {
                    "expired".to_string()
                } else {
                    quote.status
                },
                expires_at: quote.expires_at,
            })
            .collect(),
        Err(e) => {
            tracing::error!(error = %e, "failed to load RFQ quotes");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not load RFQ");
        }
    };

    let counterparties = match serde_json::from_str::<Vec<String>>(&row.counterparties) {
        Ok(wallets) => wallets,
        Err(e) => {
            tracing::error!(error = %e, "stored RFQ counterparties are invalid");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "could not load RFQ");
        }
    };
    let now = crate::quotes::now_secs() as i64;
    let status = if row.status == "open" && row.expires_at <= now {
        "expired"
    } else {
        row.status.as_str()
    };
    let is_requester = wallet == row.requester_wallet;
    let response_status = status.to_string();
    (
        StatusCode::OK,
        Json(RfqResponse {
            id: row.id,
            requester_wallet: row.requester_wallet,
            series: row.series,
            side: row.side,
            contracts: row.contracts,
            counterparties: if is_requester {
                counterparties
            } else {
                Vec::new()
            },
            status: response_status,
            expires_at: row.expires_at,
            accepted_quote_id: row.accepted_quote_id,
            quotes,
            settlement: "off_chain_agreement_only",
        }),
    )
        .into_response()
}
