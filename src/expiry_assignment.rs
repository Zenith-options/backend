//! Deterministic exercise-by-exception decisions and writer allocation.
//!
//! This module computes auditable backend instructions. The expiry keeper must
//! still submit and confirm the corresponding on-chain settlement transaction.

use sqlx::SqlitePool;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionRight {
    Call,
    Put,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortPosition {
    pub position_id: String,
    pub open_contracts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterAssignment {
    pub position_id: String,
    pub assigned_contracts: u64,
}

#[derive(Debug, Clone)]
pub struct ConfirmedExpiryRun {
    pub id: String,
    pub series_id: String,
    pub underlying: String,
    pub expires_at: String,
    pub fixing_price: f64,
    pub minimum_intrinsic: f64,
    pub chain_tx_hash: String,
}

#[derive(Debug, Clone)]
pub struct ExerciseDecision {
    pub position_id: String,
    pub exercised_contracts: u64,
    pub intrinsic_per_contract: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignmentError {
    InvalidPriceOrThreshold,
    NoOpenShortPositions,
    DuplicatePositionId,
    InsufficientWriterCapacity,
    ArithmeticOverflow,
}

/// Exercise an option when its intrinsic value per contract reaches the
/// configured threshold. Equality is in-the-money for exception purposes.
pub fn exercise_by_exception(
    now: i64,
    expires_at: i64,
    right: OptionRight,
    spot: f64,
    strike: f64,
    minimum_intrinsic: f64,
) -> Result<bool, AssignmentError> {
    if now < 0
        || !spot.is_finite()
        || !strike.is_finite()
        || !minimum_intrinsic.is_finite()
        || spot < 0.0
        || strike <= 0.0
        || minimum_intrinsic < 0.0
        || expires_at < 0
    {
        return Err(AssignmentError::InvalidPriceOrThreshold);
    }
    if now < expires_at {
        return Ok(false);
    }
    let intrinsic = match right {
        OptionRight::Call => (spot - strike).max(0.0),
        OptionRight::Put => (strike - spot).max(0.0),
    };
    Ok(intrinsic >= minimum_intrinsic && intrinsic > 0.0)
}

/// Allocate exercised contracts pro rata to open short capacity. Integer
/// remainders go to the positions with the largest fractional remainder;
/// ties are resolved by a stable SHA-256 order scoped to series and expiry.
pub fn allocate_writers(
    series_id: &str,
    expiry: &str,
    exercised_contracts: u64,
    writers: &[ShortPosition],
) -> Result<Vec<WriterAssignment>, AssignmentError> {
    if exercised_contracts == 0 {
        return Ok(Vec::new());
    }
    if writers.is_empty() || writers.iter().any(|writer| writer.open_contracts == 0) {
        return Err(AssignmentError::NoOpenShortPositions);
    }

    let mut seen = HashSet::with_capacity(writers.len());
    if writers.iter().any(|writer| !seen.insert(&writer.position_id)) {
        return Err(AssignmentError::DuplicatePositionId);
    }

    let total_capacity = writers.iter().try_fold(0u64, |sum, writer| {
        sum.checked_add(writer.open_contracts)
            .ok_or(AssignmentError::ArithmeticOverflow)
    })?;
    if exercised_contracts > total_capacity {
        return Err(AssignmentError::InsufficientWriterCapacity);
    }

    let mut allocations = Vec::with_capacity(writers.len());
    let mut allocated = 0u64;
    for writer in writers {
        let numerator = u128::from(exercised_contracts)
            .checked_mul(u128::from(writer.open_contracts))
            .ok_or(AssignmentError::ArithmeticOverflow)?;
        let base = u64::try_from(numerator / u128::from(total_capacity))
            .map_err(|_| AssignmentError::ArithmeticOverflow)?;
        let remainder = numerator % u128::from(total_capacity);
        allocated = allocated
            .checked_add(base)
            .ok_or(AssignmentError::ArithmeticOverflow)?;
        allocations.push((writer, base, remainder));
    }

    let leftover = exercised_contracts
        .checked_sub(allocated)
        .ok_or(AssignmentError::ArithmeticOverflow)?;
    allocations.sort_by(|(left, _, left_rem), (right, _, right_rem)| {
        right_rem
            .cmp(left_rem)
            .then_with(|| assignment_tiebreak(series_id, expiry, &left.position_id)
                .cmp(&assignment_tiebreak(series_id, expiry, &right.position_id)))
    });
    for (_, assigned, _) in allocations.iter_mut().take(leftover as usize) {
        *assigned += 1;
    }

    let mut result: Vec<_> = allocations
        .into_iter()
        .filter(|(_, assigned, _)| *assigned > 0)
        .map(|(writer, assigned, _)| WriterAssignment {
            position_id: writer.position_id.clone(),
            assigned_contracts: assigned,
        })
        .collect();
    result.sort_by(|left, right| left.position_id.cmp(&right.position_id));
    Ok(result)
}

fn assignment_tiebreak(series_id: &str, expiry: &str, position_id: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(series_id.as_bytes());
    hash.update([0]);
    hash.update(expiry.as_bytes());
    hash.update([0]);
    hash.update(position_id.as_bytes());
    hash.finalize().into()
}

/// Persist decisions and allocations only after a trusted chain-event verifier
/// confirms the settlement transaction. All audit rows commit together.
pub async fn persist_confirmed_run(
    pool: &SqlitePool,
    run: &ConfirmedExpiryRun,
    decisions: &[ExerciseDecision],
    assignments: &[WriterAssignment],
) -> Result<(), sqlx::Error> {
    if run.id.trim().is_empty()
        || run.series_id.trim().is_empty()
        || run.underlying.trim().is_empty()
        || run.expires_at.trim().is_empty()
        || run.chain_tx_hash.trim().is_empty()
        || !run.fixing_price.is_finite()
        || run.fixing_price < 0.0
        || !run.minimum_intrinsic.is_finite()
        || run.minimum_intrinsic < 0.0
    {
        return Err(sqlx::Error::Protocol(
            "invalid expiry settlement run".into(),
        ));
    }
    let mut decision_ids = HashSet::with_capacity(decisions.len());
    let mut exercised = 0u64;
    for decision in decisions {
        if decision.position_id.trim().is_empty()
            || decision.exercised_contracts == 0
            || !decision.intrinsic_per_contract.is_finite()
            || decision.intrinsic_per_contract <= 0.0
            || !decision_ids.insert(&decision.position_id)
        {
            return Err(sqlx::Error::Protocol(
                "invalid or duplicate exercise decision".into(),
            ));
        }
        exercised = exercised
            .checked_add(decision.exercised_contracts)
            .ok_or_else(|| sqlx::Error::Protocol("exercise count overflow".into()))?;
    }
    let mut assignment_ids = HashSet::with_capacity(assignments.len());
    let mut assigned = 0u64;
    for assignment in assignments {
        if assignment.position_id.trim().is_empty()
            || assignment.assigned_contracts == 0
            || !assignment_ids.insert(&assignment.position_id)
        {
            return Err(sqlx::Error::Protocol(
                "invalid or duplicate writer assignment".into(),
            ));
        }
        assigned = assigned
            .checked_add(assignment.assigned_contracts)
            .ok_or_else(|| sqlx::Error::Protocol("assignment count overflow".into()))?;
    }
    if exercised != assigned {
        return Err(sqlx::Error::Protocol(
            "exercise and writer assignment totals differ".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO expiry_settlement_runs
            (id, series_id, underlying, expires_at, fixing_price,
             minimum_intrinsic, chain_tx_hash)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&run.id)
    .bind(&run.series_id)
    .bind(&run.underlying)
    .bind(&run.expires_at)
    .bind(run.fixing_price)
    .bind(run.minimum_intrinsic)
    .bind(&run.chain_tx_hash)
    .execute(&mut *tx)
    .await?;
    for decision in decisions {
        sqlx::query(
            "INSERT INTO expiry_exercise_decisions
                (run_id, long_position_id, exercised_contracts, intrinsic_per_contract)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&run.id)
        .bind(&decision.position_id)
        .bind(i64::try_from(decision.exercised_contracts)
            .map_err(|_| sqlx::Error::Protocol("exercise count overflow".into()))?)
        .bind(decision.intrinsic_per_contract)
        .execute(&mut *tx)
        .await?;
    }
    for assignment in assignments {
        sqlx::query(
            "INSERT INTO expiry_writer_assignments
                (run_id, short_position_id, assigned_contracts)
             VALUES (?, ?, ?)",
        )
        .bind(&run.id)
        .bind(&assignment.position_id)
        .bind(i64::try_from(assignment.assigned_contracts)
            .map_err(|_| sqlx::Error::Protocol("assignment count overflow".into()))?)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exercise_by_exception_respects_option_right_and_threshold() {
        assert_eq!(
            exercise_by_exception(100, 100, OptionRight::Call, 110.0, 100.0, 10.0),
            Ok(true)
        );
        assert_eq!(
            exercise_by_exception(100, 100, OptionRight::Put, 90.0, 100.0, 10.01),
            Ok(false)
        );
        assert_eq!(
            exercise_by_exception(100, 100, OptionRight::Call, 99.0, 100.0, 0.0),
            Ok(false)
        );
        assert_eq!(
            exercise_by_exception(99, 100, OptionRight::Call, 110.0, 100.0, 0.0),
            Ok(false)
        );
    }

    #[test]
    fn writer_allocation_is_pro_rata_bounded_and_reproducible() {
        let writers = vec![
            ShortPosition { position_id: "a".into(), open_contracts: 2 },
            ShortPosition { position_id: "b".into(), open_contracts: 4 },
            ShortPosition { position_id: "c".into(), open_contracts: 6 },
        ];
        let first = allocate_writers("series", "expiry", 6, &writers).unwrap();
        let second = allocate_writers("series", "expiry", 6, &writers).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|a| a.assigned_contracts).sum::<u64>(),
            6
        );
        for assignment in &first {
            let capacity = writers.iter().find(|w| w.position_id == assignment.position_id).unwrap().open_contracts;
            assert!(assignment.assigned_contracts <= capacity);
        }
        assert_eq!(
            first.iter().map(|a| (&*a.position_id, a.assigned_contracts)).collect::<Vec<_>>(),
            vec![("a", 1), ("b", 2), ("c", 3)]
        );
    }

    #[test]
    fn rejects_duplicate_positions_and_excessive_assignments() {
        let duplicate = vec![
            ShortPosition { position_id: "same".into(), open_contracts: 1 },
            ShortPosition { position_id: "same".into(), open_contracts: 1 },
        ];
        assert_eq!(
            allocate_writers("s", "e", 1, &duplicate),
            Err(AssignmentError::DuplicatePositionId)
        );
        assert_eq!(
            allocate_writers(
                "s",
                "e",
                3,
                &[ShortPosition { position_id: "a".into(), open_contracts: 2 }]
            ),
            Err(AssignmentError::InsufficientWriterCapacity)
        );
    }
}
