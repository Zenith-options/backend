//! Validated multi-leg strategy bundles for atomic contract submission.
//!
//! This module validates strategy shape and margin, then stores the complete
//! bundle in one database transaction. The contract adapter must submit all
//! persisted legs in one atomic on-chain operation.

use sqlx::SqlitePool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    Spread,
    Straddle,
    Butterfly,
    CoveredCall,
}

impl StrategyKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spread => "spread",
            Self::Straddle => "straddle",
            Self::Butterfly => "butterfly",
            Self::CoveredCall => "covered_call",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionType {
    Call,
    Put,
}

impl OptionType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Put => "put",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
        }
    }

    fn sign(self) -> f64 {
        match self {
            Self::Long => 1.0,
            Self::Short => -1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrategyLeg {
    pub option_type: OptionType,
    pub side: Side,
    pub strike: f64,
    pub contracts: f64,
    pub premium: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrategyDefinition {
    pub kind: StrategyKind,
    pub underlying: String,
    pub expiry: String,
    pub spot: f64,
    pub contract_multiplier: f64,
    /// Shares held and locked for a covered call.
    pub covered_units: f64,
    pub legs: Vec<StrategyLeg>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StrategyValidationError {
    InvalidMarketOrLeg,
    InvalidStrategyShape,
    UnboundedDownside,
    InsufficientMargin { required: f64 },
}

fn close_enough(left: f64, right: f64) -> bool {
    (left - right).abs() <= 1e-9 * left.abs().max(right.abs()).max(1.0)
}

fn valid_shape(definition: &StrategyDefinition) -> bool {
    match definition.kind {
        StrategyKind::Spread if definition.legs.len() == 2 => {
            let [a, b] = definition.legs.as_slice() else {
                return false;
            };
            a.option_type == b.option_type
                && a.side != b.side
                && !close_enough(a.strike, b.strike)
                && close_enough(a.contracts, b.contracts)
        }
        StrategyKind::Straddle if definition.legs.len() == 2 => {
            let [a, b] = definition.legs.as_slice() else {
                return false;
            };
            a.option_type != b.option_type
                && a.side == b.side
                && close_enough(a.strike, b.strike)
                && close_enough(a.contracts, b.contracts)
        }
        StrategyKind::Butterfly if definition.legs.len() == 3 => {
            let mut legs = definition.legs.iter().collect::<Vec<_>>();
            legs.sort_by(|a, b| a.strike.total_cmp(&b.strike));
            let [low, middle, high] = legs.as_slice() else {
                return false;
            };
            low.option_type == middle.option_type
                && middle.option_type == high.option_type
                && low.side == high.side
                && low.side != middle.side
                && close_enough(middle.strike - low.strike, high.strike - middle.strike)
                && close_enough(low.contracts, high.contracts)
                && close_enough(middle.contracts, 2.0 * low.contracts)
        }
        StrategyKind::CoveredCall if definition.legs.len() == 1 => {
            let leg = &definition.legs[0];
            leg.option_type == OptionType::Call
                && leg.side == Side::Short
                && definition.covered_units >= leg.contracts * definition.contract_multiplier
        }
        _ => false,
    }
}

fn pnl_at(definition: &StrategyDefinition, price: f64) -> f64 {
    let option_pnl: f64 = definition
        .legs
        .iter()
        .map(|leg| {
            let intrinsic = match leg.option_type {
                OptionType::Call => (price - leg.strike).max(0.0),
                OptionType::Put => (leg.strike - price).max(0.0),
            };
            (leg.side.sign() * intrinsic - leg.side.sign() * leg.premium)
                * leg.contracts
                * definition.contract_multiplier
        })
        .sum();
    let underlying_pnl = definition.covered_units * (price - definition.spot);
    option_pnl + underlying_pnl
}

/// Validate a supported strategy structure and compute its conservative
/// expiration max-loss margin. Reject uncovered short-call tail risk instead
/// of silently falling back to a heuristic notional margin.
pub fn validate_strategy(
    definition: &StrategyDefinition,
    available_margin: f64,
) -> Result<f64, StrategyValidationError> {
    if definition.underlying.trim().is_empty()
        || definition.expiry.trim().is_empty()
        || !definition.spot.is_finite()
        || definition.spot <= 0.0
        || !definition.contract_multiplier.is_finite()
        || definition.contract_multiplier <= 0.0
        || !definition.covered_units.is_finite()
        || definition.covered_units < 0.0
        || !available_margin.is_finite()
        || available_margin < 0.0
        || definition.legs.iter().any(|leg| {
            !leg.strike.is_finite()
                || leg.strike <= 0.0
                || !leg.contracts.is_finite()
                || leg.contracts <= 0.0
                || !leg.premium.is_finite()
                || leg.premium < 0.0
        })
    {
        return Err(StrategyValidationError::InvalidMarketOrLeg);
    }
    if !valid_shape(definition) {
        return Err(StrategyValidationError::InvalidStrategyShape);
    }

    let call_tail_slope: f64 = definition
        .legs
        .iter()
        .filter(|leg| leg.option_type == OptionType::Call)
        .map(|leg| leg.side.sign() * leg.contracts * definition.contract_multiplier)
        .sum::<f64>()
        + definition.covered_units;
    if call_tail_slope < -1e-9 {
        return Err(StrategyValidationError::UnboundedDownside);
    }

    let mut points = Vec::with_capacity(definition.legs.len() + 1);
    points.push(0.0);
    points.extend(definition.legs.iter().map(|leg| leg.strike));
    let worst_pnl = points
        .into_iter()
        .map(|price| pnl_at(definition, price))
        .fold(f64::INFINITY, f64::min);
    let required = (-worst_pnl)
        .max(0.0)
        .max(definition.covered_units * definition.spot);
    if !required.is_finite() {
        return Err(StrategyValidationError::InvalidMarketOrLeg);
    }
    if required > available_margin {
        return Err(StrategyValidationError::InsufficientMargin { required });
    }
    Ok(required)
}

/// Persist the fully validated bundle atomically. It is not an on-chain
/// submission; callers must pass the resulting order as one contract call.
pub async fn persist_strategy_bundle(
    pool: &SqlitePool,
    order_id: &str,
    wallet: &str,
    idempotency_key: &str,
    definition: &StrategyDefinition,
    available_margin: f64,
) -> Result<f64, sqlx::Error> {
    let required = validate_strategy(definition, available_margin)
        .map_err(|error| sqlx::Error::Protocol(format!("strategy validation failed: {error:?}")))?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO atomic_strategy_orders
            (id, wallet_address, idempotency_key, strategy_type, underlying, expiry,
             spot, contract_multiplier, covered_units, margin_required)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(order_id)
    .bind(wallet)
    .bind(idempotency_key)
    .bind(definition.kind.as_str())
    .bind(&definition.underlying)
    .bind(&definition.expiry)
    .bind(definition.spot)
    .bind(definition.contract_multiplier)
    .bind(definition.covered_units)
    .bind(required)
    .execute(&mut *tx)
    .await?;

    for (index, leg) in definition.legs.iter().enumerate() {
        sqlx::query(
            "INSERT INTO atomic_strategy_order_legs
                (order_id, leg_index, option_type, side, strike, contracts, premium)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(order_id)
        .bind(
            i64::try_from(index)
                .map_err(|_| sqlx::Error::Protocol("too many strategy legs".into()))?,
        )
        .bind(leg.option_type.as_str())
        .bind(leg.side.as_str())
        .bind(leg.strike)
        .bind(leg.contracts)
        .bind(leg.premium)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(required)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(option_type: OptionType, side: Side, strike: f64, contracts: f64) -> StrategyLeg {
        StrategyLeg {
            option_type,
            side,
            strike,
            contracts,
            premium: 1.0,
        }
    }

    fn definition(kind: StrategyKind, legs: Vec<StrategyLeg>) -> StrategyDefinition {
        StrategyDefinition {
            kind,
            underlying: "BTC".into(),
            expiry: "2030-01-01T00:00:00Z".into(),
            spot: 100.0,
            contract_multiplier: 1.0,
            covered_units: 0.0,
            legs,
        }
    }

    #[test]
    fn validates_defined_risk_spread_and_rejects_insufficient_margin() {
        let mut spread = definition(
            StrategyKind::Spread,
            vec![
                leg(OptionType::Call, Side::Long, 100.0, 1.0),
                leg(OptionType::Call, Side::Short, 110.0, 1.0),
            ],
        );
        spread.legs[0].premium = 2.0;
        let margin = validate_strategy(&spread, 100.0).unwrap();
        assert!(margin > 0.0 && margin <= 10.0);
        assert!(matches!(
            validate_strategy(&spread, 0.0),
            Err(StrategyValidationError::InsufficientMargin { .. })
        ));
    }

    #[test]
    fn validates_straddle_butterfly_and_stock_covered_call() {
        let straddle = definition(
            StrategyKind::Straddle,
            vec![
                leg(OptionType::Call, Side::Long, 100.0, 1.0),
                leg(OptionType::Put, Side::Long, 100.0, 1.0),
            ],
        );
        assert_eq!(validate_strategy(&straddle, 100.0), Ok(2.0));

        let butterfly = definition(
            StrategyKind::Butterfly,
            vec![
                leg(OptionType::Call, Side::Long, 90.0, 1.0),
                leg(OptionType::Call, Side::Short, 100.0, 2.0),
                leg(OptionType::Call, Side::Long, 110.0, 1.0),
            ],
        );
        assert!(validate_strategy(&butterfly, 100.0).is_ok());

        let mut covered_call = definition(
            StrategyKind::CoveredCall,
            vec![leg(OptionType::Call, Side::Short, 110.0, 1.0)],
        );
        covered_call.covered_units = 1.0;
        assert!(validate_strategy(&covered_call, 100.0).is_ok());
        covered_call.covered_units = 0.99;
        assert_eq!(
            validate_strategy(&covered_call, 100.0),
            Err(StrategyValidationError::InvalidStrategyShape)
        );
    }

    #[test]
    fn rejects_a_naked_short_call_spread_with_unbounded_loss() {
        let spread = definition(
            StrategyKind::Spread,
            vec![
                leg(OptionType::Call, Side::Short, 100.0, 1.0),
                leg(OptionType::Call, Side::Long, 110.0, 0.5),
            ],
        );
        assert_eq!(
            validate_strategy(&spread, 1_000.0),
            Err(StrategyValidationError::UnboundedDownside)
        );
    }
}
