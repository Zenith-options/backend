use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;

#[derive(Debug, Clone, Deserialize)]
pub struct PayoffPreviewRequest {
    pub payoff_kind: Option<String>,
    pub series_id: Option<i64>,
    pub underlying_price: f64,
    pub strike: f64,
    pub option_type: String,
    #[serde(default = "default_contracts")]
    pub contracts: f64,
    #[serde(default)]
    pub path_high: Option<f64>,
    #[serde(default)]
    pub path_low: Option<f64>,
}

fn default_contracts() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize)]
pub struct PayoffPreview {
    pub series_id: Option<i64>,
    pub payoff_kind: String,
    pub underlying_price: f64,
    pub payoff_per_contract: f64,
    pub total_payoff: f64,
}

#[derive(Debug, Deserialize)]
pub struct ConfigurePayoffRequest {
    pub config: Value,
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetSeriesPayoffRequest {
    pub payoff_kind: String,
    pub config: Value,
}

pub async fn configure_payoff(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ConfigurePayoffRequest>,
) -> Result<StatusCode, StatusCode> {
    crate::series_lifecycle::require_operator(&headers)?;
    let formula = formula_for_kind(&kind).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let config = serde_json::to_string(&request.config).map_err(|e| {
        tracing::error!(error = %e, "failed to serialize payoff configuration");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    let result = sqlx::query(
        "UPDATE payoff_formula_registry
         SET formula = ?1, config = ?2, enabled = ?3,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE payoff_kind = ?4",
    )
    .bind(formula)
    .bind(config)
    .bind(request.enabled as i64)
    .bind(kind)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "failed to configure payoff formula");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_series_payoff(
    State(state): State<AppState>,
    Path(series_id): Path<i64>,
    headers: HeaderMap,
    Json(request): Json<SetSeriesPayoffRequest>,
) -> Result<StatusCode, StatusCode> {
    crate::series_lifecycle::require_operator(&headers)?;
    if formula_for_kind(&request.payoff_kind).is_none() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let config = serde_json::to_string(&request.config).map_err(|e| {
        tracing::error!(error = %e, series_id, "failed to serialize series payoff configuration");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    let enabled = sqlx::query_scalar::<_, i64>(
        "SELECT enabled FROM payoff_formula_registry WHERE payoff_kind = ?1",
    )
    .bind(&request.payoff_kind)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id, "failed to resolve registered payoff");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if enabled != Some(1) {
        return Err(StatusCode::CONFLICT);
    }
    let result = sqlx::query(
        "UPDATE option_series SET payoff_kind = ?1, payoff_config = ?2 WHERE id = ?3",
    )
    .bind(request.payoff_kind)
    .bind(config)
    .bind(series_id)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, series_id, "failed to set series payoff");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn post_payoff(
    State(state): State<AppState>,
    Json(request): Json<PayoffPreviewRequest>,
) -> Result<Json<PayoffPreview>, StatusCode> {
    if !request.underlying_price.is_finite()
        || request.underlying_price < 0.0
        || !request.strike.is_finite()
        || request.strike <= 0.0
        || !request.contracts.is_finite()
        || request.contracts <= 0.0
        || !matches!(request.option_type.as_str(), "call" | "put")
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let (kind, config) = if let Some(series_id) = request.series_id {
        let row = sqlx::query_as::<_, (String, String)>(
            "SELECT payoff_kind, payoff_config FROM option_series WHERE id = ?1",
        )
        .bind(series_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, series_id, "failed to load series payoff");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
        row
    } else {
        (
            request
                .payoff_kind
                .clone()
                .unwrap_or_else(|| "vanilla".to_string()),
            "{}".to_string(),
        )
    };

    let registered = sqlx::query_as::<_, (i64, String)>(
        "SELECT enabled, config FROM payoff_formula_registry WHERE payoff_kind = ?1",
    )
    .bind(&kind)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, payoff_kind = %kind, "failed to load payoff registry entry");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let Some((enabled, registered_config)) = registered else {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    };
    if enabled != 1 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let config = if config == "{}" {
        registered_config
    } else {
        config
    };
    let config: Value = serde_json::from_str(&config).map_err(|e| {
        tracing::error!(error = %e, payoff_kind = %kind, "invalid registered payoff configuration");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let per_contract = evaluate_payoff(
        &kind,
        &config,
        request.underlying_price,
        request.strike,
        request.option_type == "call",
        request.path_high,
        request.path_low,
    )?;
    let total = per_contract * request.contracts;
    if !per_contract.is_finite() || !total.is_finite() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    Ok(Json(PayoffPreview {
        series_id: request.series_id,
        payoff_kind: kind,
        underlying_price: request.underlying_price,
        payoff_per_contract: per_contract,
        total_payoff: total,
    }))
}

pub fn evaluate_payoff(
    kind: &str,
    config: &Value,
    underlying_price: f64,
    strike: f64,
    is_call: bool,
    path_high: Option<f64>,
    path_low: Option<f64>,
) -> Result<f64, StatusCode> {
    if !underlying_price.is_finite()
        || underlying_price < 0.0
        || !strike.is_finite()
        || strike <= 0.0
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let intrinsic_value = intrinsic(underlying_price, strike, is_call);
    let payoff = match kind {
        "vanilla" => intrinsic_value,
        "digital" => {
            let cash = config
                .get("cash")
                .and_then(Value::as_f64)
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            if !cash.is_finite() || cash < 0.0 {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            let in_the_money = intrinsic_value > 0.0;
            if in_the_money { cash } else { 0.0 }
        }
        "barrier" => {
            let barrier = config
                .get("barrier")
                .and_then(Value::as_f64)
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let direction = config
                .get("direction")
                .and_then(Value::as_str)
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let knock_out = config
                .get("knock_out")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if path_high.is_some_and(|v| !v.is_finite())
                || path_low.is_some_and(|v| !v.is_finite())
            {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            if !barrier.is_finite() || barrier <= 0.0 {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            let breached = match direction {
                "up" => {
                    let high = path_high.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
                    if high < underlying_price {
                        return Err(StatusCode::UNPROCESSABLE_ENTITY);
                    }
                    high >= barrier
                }
                "down" => {
                    let low = path_low.ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
                    if low > underlying_price {
                        return Err(StatusCode::UNPROCESSABLE_ENTITY);
                    }
                    low <= barrier
                }
                _ => return Err(StatusCode::UNPROCESSABLE_ENTITY),
            };
            if breached == knock_out {
                0.0
            } else {
                intrinsic_value
            }
        }
        "spread" => {
            let legs = config
                .get("legs")
                .and_then(Value::as_array)
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            if legs.is_empty() || legs.len() > 32 {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            let mut total = 0.0;
            for leg in legs {
                let strike = leg
                    .get("strike")
                    .and_then(Value::as_f64)
                    .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
                let quantity = leg
                    .get("quantity")
                    .and_then(Value::as_f64)
                    .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
                let call = match leg.get("option_type").and_then(Value::as_str) {
                    Some("call") => true,
                    Some("put") => false,
                    _ => return Err(StatusCode::UNPROCESSABLE_ENTITY),
                };
                if !strike.is_finite() || strike <= 0.0 || !quantity.is_finite() {
                    return Err(StatusCode::UNPROCESSABLE_ENTITY);
                }
                total += intrinsic(underlying_price, strike, call) * quantity;
            }
            total
        }
        "capped" => {
            let cap = config
                .get("cap")
                .and_then(Value::as_f64)
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            let floor = config.get("floor").and_then(Value::as_f64).unwrap_or(0.0);
            if !floor.is_finite() || floor < 0.0 || !cap.is_finite() || cap < floor {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            intrinsic_value.max(floor).min(cap)
        }
        "floor" => {
            let floor = config
                .get("floor")
                .and_then(Value::as_f64)
                .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
            if !floor.is_finite() || floor < 0.0 {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            intrinsic_value.max(floor)
        }
        _ => return Err(StatusCode::UNPROCESSABLE_ENTITY),
    };
    if !payoff.is_finite() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    Ok(payoff)
}

fn intrinsic(underlying_price: f64, strike: f64, is_call: bool) -> f64 {
    if is_call {
        (underlying_price - strike).max(0.0)
    } else {
        (strike - underlying_price).max(0.0)
    }
}

fn formula_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "vanilla" => Some("intrinsic"),
        "digital" => Some("cash_or_nothing"),
        "barrier" => Some("barrier_knock"),
        "spread" => Some("multi_leg"),
        "capped" => Some("capped_intrinsic"),
        "floor" => Some("floored_intrinsic"),
        _ => None,
    }
}