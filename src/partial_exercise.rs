use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::auth::AuthUser;
use crate::error::{db_error, AppError};
use crate::models::Position;
use crate::AppState;

const FIXED_SCALE: i128 = 10_000_000;
const FIXED_SCALE_F64: f64 = 10_000_000.0;

#[derive(Debug, Deserialize)]
pub struct ExerciseRequest {
    /// Client-generated idempotency identifier; retries return the original result.
    pub request_id: String,
    /// Quantity to exercise, with at most seven decimal places.
    pub contracts: f64,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ExerciseReceipt {
    pub request_id: String,
    pub position_id: String,
    pub contracts: f64,
    pub fixing_price: f64,
    pub payout: f64,
    pub realized_pnl: f64,
    pub collateral_released: f64,
}

#[derive(Debug, FromRow)]
struct StoredExerciseReceipt {
    request_id: String,
    position_id: String,
    contracts_scaled: i64,
    fixing_price_scaled: i64,
    payout_scaled: i64,
    realized_pnl_scaled: i64,
    collateral_released_scaled: i64,
}

impl From<StoredExerciseReceipt> for ExerciseReceipt {
    fn from(row: StoredExerciseReceipt) -> Self {
        Self {
            request_id: row.request_id,
            position_id: row.position_id,
            contracts: row.contracts_scaled as f64 / FIXED_SCALE_F64,
            fixing_price: row.fixing_price_scaled as f64 / FIXED_SCALE_F64,
            payout: row.payout_scaled as f64 / FIXED_SCALE_F64,
            realized_pnl: row.realized_pnl_scaled as f64 / FIXED_SCALE_F64,
            collateral_released: row.collateral_released_scaled as f64 / FIXED_SCALE_F64,
        }
    }
}

pub async fn exercise_position(
    State(state): State<AppState>,
    AuthUser(wallet): AuthUser,
    Path(position_id): Path<String>,
    Json(request): Json<ExerciseRequest>,
) -> Result<Json<ExerciseReceipt>, AppError> {
    let quantity_scaled = normalized_quantity(request.contracts)?;
    if request.request_id.trim().is_empty() || request.request_id.len() > 128 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "request_id must contain between 1 and 128 characters",
        ));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|error| db_error("begin partial exercise", error))?;

    if let Some(existing) = sqlx::query_as::<_, StoredExerciseReceipt>(
        "SELECT request_id, position_id, contracts_scaled, fixing_price_scaled, payout_scaled,
                realized_pnl_scaled, collateral_released_scaled
         FROM partial_exercises WHERE request_id = ? AND wallet_address = ?",
    )
    .bind(&request.request_id)
    .bind(&wallet)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| db_error("check partial exercise idempotency", error))?
    {
        tx.rollback()
            .await
            .map_err(|error| db_error("close duplicate exercise transaction", error))?;
        return Ok(Json(existing.into()));
    }

    let position: Position = sqlx::query_as(
        "SELECT * FROM positions
         WHERE id = ? AND wallet_address = ? AND status = 'open'",
    )
    .bind(&position_id)
    .bind(&wallet)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| db_error("load position for partial exercise", error))?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "open position not found"))?;

    if position.position_type != "long" {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "only long option positions can be exercised by the holder",
        ));
    }

    let now = current_unix_seconds()?;
    let expiry = crate::positions::parse_iso8601_utc(&position.expires_at).ok_or_else(|| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "position has an invalid expiry timestamp",
        )
    })?;
    if now < expiry {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "position cannot be exercised before expiry",
        ));
    }

    let fixing_price: Option<f64> = sqlx::query_scalar(
        "SELECT price FROM settlement_fixings
         WHERE underlying = ? AND expires_at = ?",
    )
    .bind(&position.underlying)
    .bind(&position.expires_at)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| db_error("load persisted settlement fixing", error))?;
    let fixing_price = fixing_price.ok_or_else(|| {
        AppError::new(
            StatusCode::CONFLICT,
            "authoritative settlement fixing is not available",
        )
    })?;
    let fixing_price_scaled = to_scaled(fixing_price, "settlement fixing")?;
    if fixing_price_scaled <= 0 {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "persisted settlement fixing is invalid",
        ));
    }

    let open_quantity_scaled = position.contracts.to_scaled_i64().map_err(|_| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "open position quantity is outside the supported range",
        )
    })?;
    if quantity_scaled > open_quantity_scaled {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "exercise quantity exceeds the open position quantity",
        ));
    }
    let collateral_scaled = position.collateral.to_scaled_i64().map_err(|_| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "position collateral is outside the supported range",
        )
    })?;
    let collateral_released_scaled = if quantity_scaled == open_quantity_scaled {
        collateral_scaled
    } else {
        multiply_scaled(
            collateral_scaled,
            quantity_scaled,
            i128::from(open_quantity_scaled),
        )?
    };
    let strike_scaled = position.strike.to_scaled_i64().map_err(|_| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "position strike is outside the supported range",
        )
    })?;
    let intrinsic_scaled = if position.option_type == "call" {
        fixing_price_scaled.checked_sub(strike_scaled)
    } else {
        strike_scaled.checked_sub(fixing_price_scaled)
    }
    .ok_or_else(|| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "fixed-point intrinsic value calculation overflowed",
        )
    })?
    .max(0);
    let payout_scaled = multiply_scaled(intrinsic_scaled, quantity_scaled, FIXED_SCALE)?;
    let entry_premium_scaled = position.entry_premium.to_scaled_i64().map_err(|_| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "position premium is outside the supported range",
        )
    })?;
    let paid_premium_scaled =
        multiply_scaled(entry_premium_scaled, quantity_scaled, FIXED_SCALE)?;
    let realized_pnl_scaled = payout_scaled.checked_sub(paid_premium_scaled).ok_or_else(|| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "fixed-point realized P&L calculation overflowed",
        )
    })?;

    let insert = sqlx::query(
        "INSERT INTO partial_exercises
            (request_id, position_id, wallet_address, contracts_scaled,
             fixing_price_scaled, payout_scaled, realized_pnl_scaled,
             collateral_released_scaled)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(request_id) DO NOTHING",
    )
    .bind(&request.request_id)
    .bind(&position_id)
    .bind(&wallet)
    .bind(quantity_scaled)
    .bind(fixing_price_scaled)
    .bind(payout_scaled)
    .bind(realized_pnl_scaled)
    .bind(collateral_released_scaled)
    .execute(&mut *tx)
    .await
    .map_err(|error| db_error("record partial exercise", error))?;

    if insert.rows_affected() == 0 {
        let existing = sqlx::query_as::<_, StoredExerciseReceipt>(
            "SELECT request_id, position_id, contracts_scaled, fixing_price_scaled,
                    payout_scaled, realized_pnl_scaled, collateral_released_scaled
             FROM partial_exercises WHERE request_id = ? AND wallet_address = ?",
        )
        .bind(&request.request_id)
        .bind(&wallet)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| db_error("load prior partial exercise", error))?
        .ok_or_else(|| {
            AppError::new(
                StatusCode::CONFLICT,
                "request_id has already been used by another wallet",
            )
        })?;
        tx.rollback()
            .await
            .map_err(|error| db_error("close duplicate exercise transaction", error))?;
        return Ok(Json(existing.into()));
    }

    let remaining_scaled = open_quantity_scaled - quantity_scaled;
    sqlx::query(
        "UPDATE accounts
         SET balance = balance + ?,
             collateral_locked = MAX(0.0, collateral_locked - ?)
         WHERE wallet_address = ?",
    )
    .bind(payout_scaled)
    .bind(collateral_released_scaled)
    .bind(&wallet)
    .execute(&mut *tx)
    .await
    .map_err(|error| db_error("credit partial exercise and release collateral", error))?;

    if remaining_scaled == 0 {
        let prior_realized_scaled: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(realized_pnl_scaled), 0)
             FROM partial_exercises
             WHERE position_id = ? AND request_id != ?",
        )
        .bind(&position_id)
        .bind(&request.request_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| db_error("sum prior partial exercise results", error))?;
        let updated = sqlx::query(
            "UPDATE positions
             SET contracts = 0, collateral = 0, status = 'expired',
                 close_premium = ?, close_spot = ?, realized_pnl = ?,
                 closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND wallet_address = ? AND status = 'open' AND contracts = ?",
        )
        .bind(intrinsic_scaled)
        .bind(fixing_price_scaled)
        .bind(
            prior_realized_scaled
                .checked_add(realized_pnl_scaled)
                .ok_or_else(|| {
                    AppError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "fixed-point exercise history overflowed",
                    )
                })?,
        )
        .bind(&position_id)
        .bind(&wallet)
        .bind(open_quantity_scaled)
        .execute(&mut *tx)
        .await
        .map_err(|error| db_error("mark fully exercised position", error))?;
        if updated.rows_affected() != 1 {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "position changed while the exercise was being processed",
            ));
        }
    } else {
        let remaining_collateral_scaled = collateral_scaled - collateral_released_scaled;
        let updated = sqlx::query(
            "UPDATE positions
             SET contracts = ?, collateral = ?
             WHERE id = ? AND wallet_address = ? AND status = 'open' AND contracts = ?",
        )
        .bind(remaining_scaled)
        .bind(remaining_collateral_scaled)
        .bind(&position_id)
        .bind(&wallet)
        .bind(open_quantity_scaled)
        .execute(&mut *tx)
        .await
        .map_err(|error| db_error("reduce partially exercised position", error))?;
        if updated.rows_affected() != 1 {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "position changed while the exercise was being processed",
            ));
        }
    }

    tx.commit()
        .await
        .map_err(|error| db_error("commit partial exercise", error))?;

    Ok(Json(ExerciseReceipt {
        request_id: request.request_id,
        position_id,
        contracts: quantity_scaled as f64 / FIXED_SCALE_F64,
        fixing_price: fixing_price_scaled as f64 / FIXED_SCALE_F64,
        payout: payout_scaled as f64 / FIXED_SCALE_F64,
        realized_pnl: realized_pnl_scaled as f64 / FIXED_SCALE_F64,
        collateral_released: collateral_released_scaled as f64 / FIXED_SCALE_F64,
    }))
}

pub(crate) fn normalized_quantity(quantity: f64) -> Result<i64, AppError> {
    if !quantity.is_finite() || quantity <= 0.0 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "contracts must be finite and greater than zero",
        ));
    }
    let scaled = quantity * FIXED_SCALE_F64;
    let rounded = scaled.round();
    if !scaled.is_finite() || (scaled - rounded).abs() > 0.000_001 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "contracts may have at most seven decimal places",
        ));
    }
    i64::try_from(rounded as i128).map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "contracts exceeds the supported range",
        )
    })
}

fn to_scaled(value: f64, label: &str) -> Result<i64, AppError> {
    if !value.is_finite() {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{label} is not finite"),
        ));
    }
    let scaled = (value * FIXED_SCALE_F64).round();
    if scaled < i64::MIN as f64 || scaled >= i64::MAX as f64 {
        return Err(AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{label} exceeds the supported range"),
        ));
    }
    Ok(scaled as i64)
}

fn multiply_scaled(left: i64, right: i64, divisor: i128) -> Result<i64, AppError> {
    let product = i128::from(left)
        .checked_mul(i128::from(right))
        .ok_or_else(|| {
            AppError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "fixed-point exercise calculation overflowed",
            )
        })?;
    i64::try_from(product / divisor).map_err(|_| {
        AppError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "fixed-point exercise result exceeds the supported range",
        )
    })
}

fn current_unix_seconds() -> Result<i64, AppError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "system clock is before the Unix epoch",
            )
        })?
        .as_secs()
        .try_into()
        .map_err(|_| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "system timestamp exceeds supported range",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantity_is_normalized_to_seven_decimal_places() {
        assert_eq!(normalized_quantity(1.234_567_8).unwrap(), 12_345_678);
    }

    #[test]
    fn quantity_with_more_than_seven_places_is_rejected() {
        assert!(normalized_quantity(0.000_000_11).is_err());
    }

    #[test]
    fn fixed_point_payout_matches_seven_decimal_on_chain_units() {
        assert_eq!(
            multiply_scaled(15_000_000, 2_000_000, FIXED_SCALE).unwrap(),
            3_000_000
        );
    }

    #[test]
    fn zero_and_non_finite_quantities_are_rejected() {
        assert!(normalized_quantity(0.0).is_err());
        assert!(normalized_quantity(f64::INFINITY).is_err());
    }
}
