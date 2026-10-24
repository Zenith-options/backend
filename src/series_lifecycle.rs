use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Serialize)]
pub struct ListedSeries {
    pub id: i64,
    pub underlying: String,
    pub strike: f64,
    pub expiry: String,
    pub option_type: String,
    pub instrument_id: String,
    pub payoff_kind: String,
}

#[derive(Debug, Deserialize)]
pub struct EarlySettlementRequest {
    pub settlement_price: f64,
    pub path_high: Option<f64>,
    pub path_low: Option<f64>,
}

pub async fn list_active_series(
    State(state): State<AppState>,
) -> Result<Json<Vec<ListedSeries>>, StatusCode> {
    let rows = sqlx::query_as::<_, (i64, String, i64, i64, String, String, String, String)>(
        "SELECT id, underlying, strike_scaled, strike_scale, expiry, option_type,
                instrument_id, payoff_kind
         FROM option_series
         WHERE status = 'active' AND trading_halted = 0 AND expiry >= date('now')
         ORDER BY underlying, expiry, strike_scaled, option_type",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "failed to list active option series");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(
        rows.into_iter()
            .map(|row| ListedSeries {
                id: row.0,
                underlying: row.1,
                strike: row.2 as f64 / 10_f64.powi(row.3.clamp(0, 12) as i32),
                expiry: row.4,
                option_type: row.5,
                instrument_id: row.6,
                payoff_kind: row.7,
            })
            .collect(),
    ))
}

pub async fn halt_series(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    require_operator(&headers)?;
    update_status(&state, id, "active", "halted").await
}

pub async fn resume_series(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    require_operator(&headers)?;
    update_status(&state, id, "halted", "active").await
}

pub async fn delist_series(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    require_operator(&headers)?;
    let result = sqlx::query(
        "UPDATE option_series SET status = 'delisted',
             delisted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1 AND status IN ('active', 'halted')",
    )
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to delist option series");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    changed_or_not_found(&state, id, result.rows_affected()).await
}

pub async fn settle_series_early(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(request): Json<EarlySettlementRequest>,
) -> Result<StatusCode, StatusCode> {
    require_operator(&headers)?;
    if !request.settlement_price.is_finite() || request.settlement_price < 0.0 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if request.path_high.is_some_and(|v| !v.is_finite())
        || request.path_low.is_some_and(|v| !v.is_finite())
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let series_payoff = sqlx::query_as::<_, (String, String)>(
        "SELECT payoff_kind, payoff_config FROM option_series
         WHERE id = ?1 AND status IN ('active', 'halted')",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to load final settlement formula");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;
    let registry_config = sqlx::query_as::<_, (i64, String)>(
        "SELECT enabled, config FROM payoff_formula_registry WHERE payoff_kind = ?1",
    )
    .bind(&series_payoff.0)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to load settlement payoff registry");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::CONFLICT)?;
    if registry_config.0 != 1 {
        return Err(StatusCode::CONFLICT);
    }
    let payoff_config: serde_json::Value = serde_json::from_str(if series_payoff.1 == "{}" {
        &registry_config.1
    } else {
        &series_payoff.1
    })
    .map_err(|e| {
        tracing::error!(error = %e, series_id = id, "invalid final settlement payoff config");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let mut tx = state.db.begin().await.map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to begin early settlement");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let open_positions = sqlx::query_as::<_, (String, String, f64, String, String, f64, f64, f64)>(
        "SELECT id, wallet_address, strike, option_type, position_type,
                contracts, entry_premium, collateral
         FROM positions WHERE series_id = ?1 AND status = 'open'",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to load positions for final settlement");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let mut wallet_adjustments: std::collections::HashMap<String, (f64, f64)> =
        std::collections::HashMap::new();
    for (position_id, wallet, strike, option_type, position_type, contracts, entry_premium, collateral) in
        &open_positions
    {
        if !strike.is_finite()
            || !contracts.is_finite()
            || !entry_premium.is_finite()
            || !collateral.is_finite()
            || *contracts <= 0.0
            || *collateral < 0.0
        {
            return Err(StatusCode::CONFLICT);
        }
        let is_call = match option_type.as_str() {
            "call" => true,
            "put" => false,
            _ => return Err(StatusCode::CONFLICT),
        };
        let payout = crate::payoff::evaluate_payoff(
            &series_payoff.0,
            &payoff_config,
            request.settlement_price,
            *strike,
            is_call,
            request.path_high,
            request.path_low,
        )?;
        let side = match position_type.as_str() {
            "long" => 1.0,
            "short" => -1.0,
            _ => return Err(StatusCode::CONFLICT),
        };
        let realized_pnl = (payout - entry_premium) * contracts * side;
        let adjustment = wallet_adjustments.entry(wallet.clone()).or_default();
        adjustment.0 += payout * contracts * side + collateral;
        adjustment.1 += collateral;
        let result = sqlx::query(
            "UPDATE positions SET status = 'closed', close_spot = ?1, close_premium = ?2,
                    realized_pnl = ?3, collateral = 0,
                    closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?4 AND status = 'open'",
        )
        .bind(request.settlement_price)
        .bind(payout)
        .bind(realized_pnl)
        .bind(position_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, position_id, "failed to settle open position");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        if result.rows_affected() != 1 {
            return Err(StatusCode::CONFLICT);
        }
    }
    for (wallet, (balance_delta, collateral_release)) in wallet_adjustments {
        sqlx::query(
            "UPDATE accounts SET balance = balance + ?1,
                    collateral_locked = MAX(0, collateral_locked - ?2)
             WHERE wallet_address = ?3",
        )
        .bind(balance_delta)
        .bind(collateral_release)
        .bind(wallet)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, series_id = id, "failed to credit final settlement");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    let result = sqlx::query(
        "UPDATE option_series
         SET status = 'settled', settlement_price = ?1,
             settled_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?2 AND status IN ('active', 'halted')",
    )
    .bind(request.settlement_price)
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to record early settlement");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if result.rows_affected() == 0 {
        tx.rollback().await.map_err(|e| {
            tracing::error!(error = %e, series_id = id, "failed to rollback empty settlement");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM option_series WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, series_id = id, "failed to verify settlement series");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        return Err(if exists == 0 {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::CONFLICT
        });
    }
    sqlx::query(
        "INSERT INTO series_settlements (series_id, settlement_price, settled_positions)
         VALUES (?1, ?2, ?3)",
    )
    .bind(id)
    .bind(request.settlement_price)
    .bind(open_positions.len() as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to persist series settlement record");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    tx.commit().await.map_err(|e| {
        tracing::error!(error = %e, series_id = id, "failed to commit early settlement");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn update_status(
    state: &AppState,
    id: i64,
    from: &str,
    to: &str,
) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query("UPDATE option_series SET status = ?1 WHERE id = ?2 AND status = ?3")
        .bind(to)
        .bind(id)
        .bind(from)
        .execute(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, series_id = id, "failed to update option series lifecycle");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    changed_or_not_found(state, id, result.rows_affected()).await
}

async fn changed_or_not_found(
    state: &AppState,
    id: i64,
    changed: u64,
) -> Result<StatusCode, StatusCode> {
    if changed == 1 {
        return Ok(StatusCode::NO_CONTENT);
    }
    let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM option_series WHERE id = ?1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, series_id = id, "failed to verify option series");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Err(if exists == 0 {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::CONFLICT
    })
}

pub(crate) fn require_operator(headers: &HeaderMap) -> Result<(), StatusCode> {
    let expected = std::env::var("OPERATOR_API_TOKEN")
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if expected.is_empty() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let supplied = headers
        .get("x-operator-token")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let mut expected_mac =
        HmacSha256::new_from_slice(b"zenith-series-operator-token-v1")
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    expected_mac.update(expected.as_bytes());
    let expected_tag = expected_mac.finalize().into_bytes();
    let mut supplied_mac =
        HmacSha256::new_from_slice(b"zenith-series-operator-token-v1")
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    supplied_mac.update(supplied.as_bytes());
    supplied_mac
        .verify_slice(&expected_tag)
        .map_err(|_| StatusCode::UNAUTHORIZED)
}
