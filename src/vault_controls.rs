//! Backend controls for vault withdrawal authorization.
//!
//! Every deposit and withdrawal confirmation must originate from a trusted
//! chain-event verifier. The vault contract must independently enforce its
//! freeze state and withdrawal limits.

use sqlx::SqlitePool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawalPolicy {
    pub window_seconds: i64,
    pub max_withdrawal: i64,
    pub cooling_seconds: i64,
}

fn invalid(message: &'static str) -> sqlx::Error {
    sqlx::Error::Protocol(message.to_owned())
}

fn cooling_period_elapsed(deposit_at: i64, now: i64, cooling_seconds: i64) -> bool {
    deposit_at
        .checked_add(cooling_seconds)
        .is_some_and(|available_at| now >= available_at)
}

fn rolling_cap_allows(already_withdrawn: i64, amount: i64, cap: i64) -> bool {
    already_withdrawn
        .checked_add(amount)
        .is_some_and(|total| total <= cap)
}

async fn require_risk_admin(
    pool: &SqlitePool,
    wallet: &str,
    session_token: &str,
) -> Result<(), sqlx::Error> {
    let allowed: Option<i64> = sqlx::query_scalar(
        "SELECT 1
         FROM admin_roles r
         JOIN sessions s ON s.wallet_address = r.wallet_address
         WHERE r.wallet_address = ?
           AND r.role IN ('risk_admin', 'super_admin')
           AND s.token = ?
           AND s.admin_step_up_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         LIMIT 1",
    )
    .bind(wallet)
    .bind(session_token)
    .fetch_optional(pool)
    .await?;
    if allowed.is_none() {
        return Err(invalid("a stepped-up risk administrator session is required"));
    }
    Ok(())
}

/// Configure rolling-window limits and deposit cooling-off for a vault.
pub async fn configure_policy(
    pool: &SqlitePool,
    vault_id: &str,
    admin_wallet: &str,
    session_token: &str,
    policy: WithdrawalPolicy,
    now: i64,
) -> Result<(), sqlx::Error> {
    if vault_id.trim().is_empty()
        || policy.window_seconds <= 0
        || policy.max_withdrawal <= 0
        || policy.cooling_seconds < 0
        || now < 0
    {
        return Err(invalid("invalid vault withdrawal policy"));
    }
    require_risk_admin(pool, admin_wallet, session_token).await?;
    sqlx::query(
        "INSERT INTO vault_withdrawal_policies
            (vault_id, window_seconds, max_withdrawal, cooling_seconds, updated_by, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(vault_id) DO UPDATE SET
            window_seconds = excluded.window_seconds,
            max_withdrawal = excluded.max_withdrawal,
            cooling_seconds = excluded.cooling_seconds,
            updated_by = excluded.updated_by,
            updated_at = excluded.updated_at",
    )
    .bind(vault_id)
    .bind(policy.window_seconds)
    .bind(policy.max_withdrawal)
    .bind(policy.cooling_seconds)
    .bind(admin_wallet)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a deposit only after a trusted verifier confirms it on-chain.
pub async fn record_confirmed_deposit(
    pool: &SqlitePool,
    deposit_id: &str,
    vault_id: &str,
    wallet: &str,
    amount: i64,
    chain_tx_hash: &str,
    confirmed_at: i64,
) -> Result<(), sqlx::Error> {
    if amount <= 0 || confirmed_at < 0 || chain_tx_hash.trim().is_empty() {
        return Err(invalid("invalid confirmed deposit"));
    }
    sqlx::query(
        "INSERT INTO vault_confirmed_deposits
            (id, vault_id, wallet_address, amount, chain_tx_hash, confirmed_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(deposit_id)
    .bind(vault_id)
    .bind(wallet)
    .bind(amount)
    .bind(chain_tx_hash)
    .bind(confirmed_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Authorize a withdrawal only when the vault is unfrozen, the most recent
/// deposit has cleared its cooling period, and the rolling cap remains intact.
pub async fn authorize_withdrawal(
    pool: &SqlitePool,
    withdrawal_id: &str,
    vault_id: &str,
    wallet: &str,
    amount: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    if amount <= 0 || now < 0 {
        return Err(invalid("withdrawal amount and time must be positive"));
    }
    let mut tx = pool.begin().await?;

    // Acquire SQLite's writer lock before reading the rolling sum so two
    // concurrent requests cannot both pass against the same stale total.
    let locked = sqlx::query(
        "UPDATE vault_withdrawal_policies SET frozen = frozen WHERE vault_id = ?",
    )
    .bind(vault_id)
    .execute(&mut *tx)
    .await?;
    if locked.rows_affected() != 1 {
        return Err(invalid("vault withdrawal policy is not configured"));
    }
    let policy = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT window_seconds, max_withdrawal, cooling_seconds, frozen
         FROM vault_withdrawal_policies WHERE vault_id = ?",
    )
    .bind(vault_id)
    .fetch_one(&mut *tx)
    .await?;
    if policy.3 != 0 {
        return Err(invalid("vault withdrawals are frozen"));
    }

    let latest_deposit: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(confirmed_at) FROM vault_confirmed_deposits
         WHERE vault_id = ? AND wallet_address = ?",
    )
    .bind(vault_id)
    .bind(wallet)
    .fetch_one(&mut *tx)
    .await?;
    if latest_deposit.is_some_and(|deposit_at| !cooling_period_elapsed(deposit_at, now, policy.2))
    {
        return Err(invalid("deposit cooling-off period has not elapsed"));
    }

    let window_start = now.saturating_sub(policy.0);
    let already_withdrawn: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0) FROM vault_withdrawals
         WHERE vault_id = ? AND wallet_address = ?
           AND requested_at >= ?
           AND status IN ('authorized', 'confirmed')",
    )
    .bind(vault_id)
    .bind(wallet)
    .bind(window_start)
    .fetch_one(&mut *tx)
    .await?;
    if !rolling_cap_allows(already_withdrawn, amount, policy.1) {
        return Err(invalid("rolling withdrawal cap would be exceeded"));
    }

    sqlx::query(
        "INSERT INTO vault_withdrawals
            (id, vault_id, wallet_address, amount, requested_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(withdrawal_id)
    .bind(vault_id)
    .bind(wallet)
    .bind(amount)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Release a pending authorization after a trusted verifier confirms the
/// contract withdrawal did not execute.
pub async fn cancel_failed_withdrawal(
    pool: &SqlitePool,
    withdrawal_id: &str,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE vault_withdrawals SET status = 'cancelled'
         WHERE id = ? AND status = 'authorized'",
    )
    .bind(withdrawal_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(invalid("withdrawal authorization is not pending"));
    }
    Ok(())
}

/// Confirm a previously authorized withdrawal after the contract event is
/// verified. Authorized and confirmed amounts count until they leave the
/// rolling window.
pub async fn confirm_withdrawal(
    pool: &SqlitePool,
    withdrawal_id: &str,
    chain_tx_hash: &str,
    confirmed_at: i64,
) -> Result<(), sqlx::Error> {
    if chain_tx_hash.trim().is_empty() || confirmed_at < 0 {
        return Err(invalid("invalid withdrawal confirmation"));
    }
    let updated = sqlx::query(
        "UPDATE vault_withdrawals
         SET status = 'confirmed', chain_tx_hash = ?, confirmed_at = ?
         WHERE id = ? AND status = 'authorized' AND requested_at <= ?",
    )
    .bind(chain_tx_hash)
    .bind(confirmed_at)
    .bind(withdrawal_id)
    .bind(confirmed_at)
    .execute(pool)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(invalid("withdrawal authorization is not pending"));
    }
    Ok(())
}

/// Freeze or unfreeze a vault using a stepped-up risk-admin session.
pub async fn set_vault_frozen(
    pool: &SqlitePool,
    event_id: &str,
    vault_id: &str,
    admin_wallet: &str,
    session_token: &str,
    frozen: bool,
    reason: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    if event_id.trim().is_empty() || reason.trim().is_empty() || now < 0 {
        return Err(invalid("freeze event id, reason, and valid time are required"));
    }
    require_risk_admin(pool, admin_wallet, session_token).await?;
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE vault_withdrawal_policies SET frozen = ? WHERE vault_id = ?",
    )
    .bind(if frozen { 1_i64 } else { 0_i64 })
    .bind(vault_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(invalid("vault withdrawal policy is not configured"));
    }
    sqlx::query(
        "INSERT INTO vault_freeze_events
            (id, vault_id, admin_wallet, frozen, reason, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(event_id)
    .bind(vault_id)
    .bind(admin_wallet)
    .bind(if frozen { 1_i64 } else { 0_i64 })
    .bind(reason)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_cooling_boundary_is_inclusive_and_overflow_fails_closed() {
        assert!(!cooling_period_elapsed(100, 109, 10));
        assert!(cooling_period_elapsed(100, 110, 10));
        assert!(!cooling_period_elapsed(i64::MAX, i64::MAX, 1));
    }

    #[test]
    fn rolling_cap_includes_exact_limit_and_rejects_overflow() {
        assert!(rolling_cap_allows(70, 30, 100));
        assert!(!rolling_cap_allows(70, 31, 100));
        assert!(!rolling_cap_allows(i64::MAX, 1, i64::MAX));
    }
}
