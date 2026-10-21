// Minimal admin module scaffold to satisfy the public API referenced by the
// task runtime and bin utilities.
use crate::error::AppError;

#[derive(Clone, Copy, Debug)]
pub enum AdminRole {
    Viewer,
    Operator,
    RiskAdmin,
}

pub fn require_admin_access(_role: AdminRole) -> Result<(), AppError> {
    Ok(())
}

pub async fn generate_fixtures(
    _db: sqlx::SqlitePool,
    _wallets: usize,
    _days: u32,
    _seed: u64,
) -> Result<(), AppError> {
    Ok(())
}

pub async fn anonymize_snapshot(_from: &str, _to: &str) -> Result<(), AppError> {
    let _ = (_from, _to);
    Ok(())
}
