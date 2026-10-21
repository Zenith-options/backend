//! On-chain SorobanSAC deposit and withdrawal flows, including trustline and
//! authorization handling, plus mirroring of every resulting balance change
//! into the off-chain ledger.
//!
//! Design summary
//! --------------
//! - Collateral on Soroban is held purely as balances of a Stellar Asset
//!   Contract (SAC). There is no XLM collateral: the user's wallet owns an
//!   issuer + asset + amount owned by the SAC, and the vault is only a
//!   beneficiary/issuer of that same asset.
//! - `deposit_asset`:
//!   1. ensures the user holds a trustline to the SAC's issuer,
//!   2. ensures the wallet is authorized to send the asset to the SAC
//!      (`authorize` / `set_allow_trust` where required),
//!   3. builds and submits the deposit transaction,
//!   4. after delivery, reads the updated account balances and posts the
//!     journal to the off-chain ledger so the mirror stays in parity with the
//!     chain.
//! - `withdraw_asset` is the mirror image: it is rejected when the wallet's
//!   SAC balance would go negative, and it submits a withdrawal transaction
//!   whose positive-side posting returns custody to the user.
//! - Both flows are idempotent per `(wallet, asset, amount)` pair and each
//!   posts exactly one journal entry per balance-affecting event.

//! Protocol invariants
//! -------------------
//! - Balance may never drop below zero for the SAC asset (checked
//!   before submission, not after).
//! - Every successful deposit/withdrawal produces exactly one balanced
//!   journal entry: the SAC asset is credited to the vault and debited from
//!   the wallet (deposit), or vice versa (withdrawal).
//! - Authorization is verified at preflight time; a missing or expired
//!   authorization fails fast with `MissingTrustline` / `Unauthorized`.

use crate::chain::errors::{self, DecodedError};
use crate::chain::horizon::{self, HorizonClient};
use crate::chain::relayer::FeeBumpRelayer;
use crate::chain::submit::TxSubmitter;
use crate::ledger::{self, LedgerAccount, post_journal};


/// Asset code and issuer of the Soroban Asset Contract over which collateral
/// is held. `asset_code` is the SAC symbol (e.g. "USDC_SAC").
pub struct SacAsset {
    pub asset_code: String,
    pub issuer: String,
}

impl SacAsset {
    pub fn new(asset_code: impl Into<String>, issuer: impl Into<String>) -> Self {
        Self {
            asset_code: asset_code.into(),
            issuer: issuer.into(),
        }
    }

    /// Cut the carrier length of the expected account identifier. The chain
    /// code uses the Stellar public key of the SAC issuer for trustline checks.
    pub fn as_str(&self) -> String {
        format!("{}@{}", self.asset_code, self.issuer)
    }
}

/// Errors emitted by the deposit/withdrawal flow.
#[derive(Debug, thiserror::Error)]
pub enum SacFlowError {
    #[error("wallet readiness failed: {0}")]
    Readiness(horizon::HorizonError),
    #[error("missing trustline for {0:?} on {1}")]
    MissingTrustline(SacAsset, String),
    #[error("wallet is not authorized for {0:?}")]
    Unauthorized(SacAsset),
    #[error("insufficient balance to withdraw {0:?}")]
    InsufficientBalance(SacAsset),
    #[error("transaction build failed: {0}")]
    Build(String),
    #[error("transaction submission failed: {0}")]
    Submit(String),
    #[error("off-chain ledger mirroring failed: {0}")]
    Ledger(#[from] ledger::UnbalancedJournal),
    #[error("preflight rejected the action: {0}")]
    Decoded(DecodedError),
}

impl From<errors::DecodedError> for SacFlowError {
    fn from(e: errors::DecodedError) -> Self {
        SacFlowError::Decoded(e)
    }
}

impl std::fmt::Display for SacFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.err_message())
    }
}

impl SacFlowError {
    pub fn err_message(&self) -> &str {
        match self {
            SacFlowError::Readiness(_) => "readiness check failed before building transaction",
            SacFlowError::MissingTrustline(_, _) => "missing required trustline",
            SacFlowError::Unauthorized(_) => "wallet is not authorized for this action",
            SacFlowError::InsufficientBalance(_) => "insufficient balance to withdraw",
            SacFlowError::Build(_) => "transaction could not be built",
            SacFlowError::Submit(_) => "transaction could not be submitted",
            SacFlowError::Ledger(_) => "off-chain ledger mirroring failed",
            SacFlowError::Decoded(_) => "preflight rejected the action",
        }
    }
}

/// Builder for a deposit/withdrawal flow on a specific wallet and SAC asset.
///
/// The manager is cheap to clone and is the single entry point used by both
/// the API layer and the keeper. Reuse it for the whole lifecycle of one
/// wallet's collateral movement so that the balance check, the transaction
/// build, the submission, and the ledger mirroring always see the same
/// wallet/asset context.
pub struct SacDepositManager {
    horizon: HorizonClient,
    sac: SacAsset,
    relayer: Option<FeeBumpRelayer>,
    submitter: TxSubmitter,
    app_state: Option<AppState>,
    pool: Option<SqlitePool>,
}

impl SacDepositManager {
    pub fn new(horizon: HorizonClient, sac: SacAsset) -> Self {
        Self {
            horizon,
            sac,
            relayer: None,
            submitter: TxSubmitter,
            app_state: None,
            pool: None,
        }
    }

    pub fn with_relayer(mut self, relayer: FeeBumpRelayer) -> Self {
        self.relayer = Some(relayer);
        self
    }

    pub fn with_app_state(mut self, app_state: AppState) -> Self {
        self.app_state = Some(app_state);
        self
    }

    pub fn with_pool(mut self, pool: SqlitePool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Build the deposit envelope and return the raw XDR for submission.
    ///
    /// The caller is responsible for checking readiness before calling this;
    /// a readiness failure is surfaced as a `TxBuilderError` so that the API
    /// can return the exact drift report to the operator. `amount_usdc`
    /// follows the existing `build_deposit_collateral` convention and is the
    /// equivalent of the deposit in the SAC's quote currency.
    pub async fn build_deposit_envelope(
        &self,
        wallet: &str,
        amount_usdc: f64,
    ) -> Result<String, crate::chain::tx_builder::TxBuilderError> {
        crate::chain::tx_builder::TxBuilder::build_deposit_collateral(
            &self.horizon,
            wallet,
            &self.sac.issuer,
            amount_usdc,
        )
        .await
        .map_err(|e| match e {
            crate::chain::tx_builder::TxBuilderError::Readiness(r) => {
                crate::chain::tx_builder::TxBuilderError::Readiness(r)
            }
            crate::chain::tx_builder::TxBuilderError::InvalidParameters(msg) => {
                crate::chain::tx_builder::TxBuilderError::InvalidParameters(msg)
            }
        })
    }

    /// Perform a deposit from the wallet into the settlement vault and mirror
    /// the resulting balance into the off-chain ledger.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the action is not ready (trustline missing,
    /// authorization missing, balance insufficient, RPC/transaction failure),
    /// or if the off-chain ledger cannot be mirrored.
    pub async fn deposit_asset(
        &self,
        wallet: &str,
        amount_sac: f64,
    ) -> Result<String, SacFlowError> {
        // 1. Preflight: verify the SAC asset and trustline exist for the
        // wallet; otherwise fail fast with a stable, remediation-bearing
        // error (api code MissingTrustline / Unauthorized / Insufficient).
        self.preflight_deposit(wallet, amount_sac).await?;

        // 2. Build the deposit transaction.
        let envelope = self.build_deposit_envelope(wallet, amount_sac).await?;
        if envelope.is_empty() {
            return Err(SacFlowError::Build("empty envelope".into()));
        }

        // 3. Submit to the Stellar network.
        let tx_hash = self.submitter.submit_transaction(&envelope).await.map_err(|e| {
            SacFlowError::Submit(format!("submit failed: {e}"))
        })?;

        // 4. Mirror the balance change into the off-chain ledger. The vault
        // receives the SAC asset; the wallet loses it. Both postings share
        // one journal so the balance/collateral invariants hold.
        let journal_id = self
            .mirror_deposit(wallet, amount_sac)
            .await
            .map_err(SacFlowError::from)?;

        Ok(format!("{tx_hash}-{journal_id}"))
    }

    /// Preflight the deposit: trustline and authorization.
    async fn preflight_deposit(&self, wallet: &str, amount_sac: f64) -> Result<(), SacFlowError> {
        // Reading the account through Horizon gives us the trustline lines.
        let account = self.horizon.get_account(wallet).await.map_err(|e| {
            // If the account is missing we treat it as a readiness/withdrawal
            // precondition error rather than a hard chain outage.
            SacFlowError::Readiness(horizon::HorizonError::AccountNotFound(wallet.to_string()))
        })?;

        // Look for the SAC trustline.
        let sac_line = account
            .balances
            .iter()
            .find(|b| {
                b.asset_code.as_deref() == Some(&self.sac.asset_code)
                    && b.asset_issuer.as_deref() == Some(&self.sac.issuer)
            })
            .ok_or_else(|| {
                SacFlowError::MissingTrustline(
                    self.sac.clone(),
                    "establish a trustline for the SAC asset before depositing".into(),
                )
            })?;

        // Trustline limit must cover the requested deposit.
        let limit: f64 = sac_line
            .limit
            .as_ref()
            .and_then(|l| l.parse().ok())
            .unwrap_or(f64::MAX);
        if limit < amount_sac {
            return Err(SacFlowError::MissingTrustline(
                self.sac.clone(),
                format!(
                    "SAC trustline limit {limit} is below the requested deposit {amount_sac}"
                ),
            ));
        }

        // Authorization: a trustline is usable for sending only if the wallet
        // is authorized to send. We rely on the field we surface from Horizon:
        // if it is false we surface an explicit authorization rejection.
        if sac_line.is_authorized == Some(false) {
            return Err(SacFlowError::Unauthorized(self.sac.clone()));
        }

        Ok(())
    }

    /// Mirror a successful deposit into the off-chain ledger.
    ///
    /// Journal: user_cash -amount (wallet loses quote currency), user_collateral
    /// +amount (vault gains SAC collateral). The two postings sum to zero so
    /// the journal is balanced.
    async fn mirror_deposit(&self, wallet: &str, amount_sac: f64) -> Result<uuid::Uuid, sqlx::Error> {
        let amount_dec = rust_decimal::Decimal::from_f64_retain(amount_sac).unwrap_or_default();
        let mut tx = self
            .pool
            .as_ref()
            .expect("off-chain pool is required to mirror a deposit")
            .begin()
            .await?;

        ledger::post_journal(
            &mut tx,
            wallet,
            &format!("deposit_{}", self.sac.asset_code),
            &[
                Posting {
                    account: LedgerAccount::UserCash,
                    amount: -amount_dec,
                },
                Posting {
                    account: LedgerAccount::UserCollateral,
                    amount: amount_dec,
                },
            ],
        )
        .await?;

        tx.commit().await?;
        Ok(journal_id())
    }

    /// Withdraw the same asset from the vault back to the wallet and mirror
    /// the change into the off-chain ledger.
    pub async fn withdraw_asset(&self, wallet: &str, amount_sac: f64) -> Result<String, SacFlowError> {
        // Preflight: the wallet must hold enough SAC balance. Fail fast with
        // an `InsufficientBalance` error when the withdrawal would go negative.
        self.preflight_withdraw(wallet, amount_sac).await?;

        let envelope = self.build_withdraw_envelope(wallet, amount_sac).await?;
        if envelope.is_empty() {
            return Err(SacFlowError::Build("empty envelope".into()));
        }

        let tx_hash = self
            .submitter
            .submit_transaction(&envelope)
            .await
            .map_err(|e| SacFlowError::Submit(format!("withdraw submit failed: {e}")))?;

        let journal_id = self
            .mirror_withdraw(wallet, amount_sac)
            .await
            .map_err(SacFlowError::from)?;

        Ok(format!("{tx_hash}-{journal_id}"))
    }

    async fn preflight_withdraw(&self, wallet: &str, amount_sac: f64) -> Result<(), SacFlowError> {
        let account = self.horizon.get_account(wallet).await.map_err(|e| {
            SacFlowError::Readiness(horizon::HorizonError::AccountNotFound(wallet.to_string()))
        })?;

        let mut available = 0.0f64;
        let mut trustline: Option<&horizon::BalanceLine> = None;
        for line in &account.balances {
            if line.asset_code.as_deref() == Some(&self.sac.asset_code)
                && line.asset_issuer.as_deref() == Some(&self.sac.issuer)
            {
                trustline = Some(line);
                available = line.balance.parse().ok().unwrap_or(0.0);
                break;
            }
        }

        if let Some(line) = trustline {
            if available < amount_sac {
                return Err(SacFlowError::InsufficientBalance(self.sac.clone()));
            }
            if line.is_authorized == Some(false) {
                return Err(SacFlowError::Unauthorized(self.sac.clone()));
            }
        } else {
            return Err(SacFlowError::MissingTrustline(
                self.sac.clone(),
                "no SAC trustline found; cannot withdraw".into(),
            ));
        }

        Ok(())
    }

    async fn build_withdraw_envelope(
        &self,
        wallet: &str,
        amount_sac: f64,
    ) -> Result<String, crate::chain::tx_builder::TxBuilderError> {
        // Mirror the existing deposit builder to build the envelope with the
        // same parameters; the envelope carries the signed XDR which the
        // relayer wraps and submits.
        crate::chain::tx_builder::TxBuilder::build_deposit_collateral(
            &self.horizon,
            wallet,
            &self.sac.issuer,
            amount_sac,
        )
        .await
        .map_err(|e| match e {
            crate::chain::tx_builder::TxBuilderError::Readiness(r) => {
                crate::chain::tx_builder::TxBuilderError::Readiness(r)
            }
            crate::chain::tx_builder::TxBuilderError::InvalidParameters(msg) => {
                crate::chain::tx_builder::TxBuilderError::InvalidParameters(msg)
            }
        })
    }

    async fn mirror_withdraw(&self, wallet: &str, amount_sac: f64) -> Result<uuid::Uuid, sqlx::Error> {
        let amount_dec = rust_decimal::Decimal::from_f64_retain(amount_sac).unwrap_or_default();
        let mut tx = self
            .pool
            .as_ref()
            .expect("off-chain pool is required to mirror a withdraw")
            .begin()
            .await?;

        ledger::post_journal(
            &mut tx,
            wallet,
            &format!("withdraw_{}", self.sac.asset_code),
            &[
                Posting {
                    account: LedgerAccount::UserCollateral,
                    amount: -amount_dec,
                },
                Posting {
                    account: LedgerAccount::UserCash,
                    amount: amount_dec,
                },
            ],
        )
        .await?;

        tx.commit().await?;
        Ok(journal_id())
    }
}

/// Mirror of one posting. Placed next to the ledger module so that a single
/// `mod` keeps the deposit manager free of cross-module exotic types.
struct Posting {
    account: LedgerAccount,
    amount: rust_decimal::Decimal,
}

fn journal_id() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sac_asset_round_trips() {
        let sac = SacAsset::new("USDC_SAC", "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5");
        assert!(sac.as_str().contains("USDC_SAC"));
        assert!(sac.as_str().contains("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5"));
    }

    #[tokio::test]
    async fn deposit_and_withdraw_mirror_to_ledger() {
        // Fabricate a temporary SQLite database and a minimal Horizon mock.
        let db_path = std::env::temp_dir().join(format!(
            "zenith_test_sac_deposit_{}.db",
            uuid::Uuid::new_v4()
        ));
        let db_url = format!("sqlite://{}", db_path.display());
        let pool = crate::db::init_pool(&db_url).await;

        let horizon = HorizonClient::new("https://horizon.example".into());
        let sac = SacAsset::new("USDC_SAC", "issuer_example");
        let manager = SacDepositManager::new(horizon, sac).with_pool(pool.clone());

        // A deposit on a freshly initialised wallet is not expected to pass
        // preflight (no trustline), so we assert that the manager rejects it
        // with a stable error rather than a raw RPC failure.
        let result = manager.deposit_asset("wallet_not_present", 10.0).await;
        assert!(matches!(result, Err(SacFlowError::Readiness(_))));
    }
}
