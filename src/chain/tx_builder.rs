//! Transaction assembly.
//!
//! The builder takes an unsigned invocation, simulates it to learn the ledger
//! footprint and the declared resources, prices the transaction (resource fee
//! from the declared resources, inclusion fee from the fee strategy) and
//! returns the envelope that still has to be signed.
//!
//! Nothing here hard-codes a footprint or a fee amount: the footprint comes
//! from the simulation and the resource fee is computed from the resources the
//! simulation declared.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::fees::{DailyFeeBudget, FeeOracle, FeeStats, InclusionFee, InclusionFeeStrategy};
use super::horizon::HorizonClient;
use super::readiness::{check_wallet_readiness, ReadinessReport};
use super::rpc::SorobanRpc;
use super::simulate::{
    resource_fee_for, ResourcePricing, SimulatedTransaction, SimulationError, SorobanSimulator,
    DEFAULT_FOOTPRINT_TTL_LEDGERS,
};
use super::types::SimulateFootprint;
use std::sync::Arc;

#[derive(Debug)]
pub enum TxBuilderError {
    ReadinessFailed(ReadinessReport),
    InvalidParameters(String),
    /// The simulation failed or returned something unusable.
    Simulation(SimulationError),
    /// The fee strategy or the daily budget refused the transaction.
    FeeStrategy(super::fees::FeeStrategyError),
}

impl std::fmt::Display for TxBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadinessFailed(_) => write!(
                f,
                "Account readiness check failed before building transaction"
            ),
            Self::InvalidParameters(msg) => write!(f, "Invalid transaction parameters: {msg}"),
            Self::Simulation(err) => write!(f, "Transaction simulation failed: {err}"),
            Self::FeeStrategy(err) => write!(f, "Fee strategy refused the transaction: {err}"),
        }
    }
}

impl std::error::Error for TxBuilderError {}

impl From<SimulationError> for TxBuilderError {
    fn from(error: SimulationError) -> Self {
        Self::Simulation(error)
    }
}

impl From<super::fees::FeeStrategyError> for TxBuilderError {
    fn from(error: super::fees::FeeStrategyError) -> Self {
        Self::FeeStrategy(error)
    }
}

/// A contract invocation to be simulated and priced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxBuildRequest {
    /// The account that will sign the transaction.
    pub source_account: String,
    /// The sequence number of the source account.
    pub sequence: i64,
    /// The contract to invoke.
    pub contract_id: String,
    /// The contract function to call.
    pub function: String,
    /// The arguments of the call.
    #[serde(default)]
    pub args: Vec<String>,
    /// The last ledger the transaction may be included in.
    #[serde(default)]
    pub valid_until_ledger: Option<u32>,
}

impl TxBuildRequest {
    fn validate(&self) -> Result<(), TxBuilderError> {
        if self.source_account.trim().is_empty() {
            return Err(TxBuilderError::InvalidParameters(
                "source account is required".into(),
            ));
        }
        if self.contract_id.trim().is_empty() {
            return Err(TxBuilderError::InvalidParameters(
                "contract id is required".into(),
            ));
        }
        if self.function.trim().is_empty() {
            return Err(TxBuilderError::InvalidParameters(
                "contract function is required".into(),
            ));
        }
        Ok(())
    }
}

/// A single operation of the envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeContractOperation {
    /// The contract to invoke.
    pub contract_id: String,
    /// The function to call.
    pub function: String,
    /// The arguments of the call.
    #[serde(default)]
    pub args: Vec<String>,
}

/// The unsigned transaction envelope.
///
/// The platform does not ship a Soroban XDR codec: the envelope is serialised
/// as canonical JSON and base64-encoded so it can be handed to the signer,
/// which turns it into the final XDR envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionEnvelope {
    /// The account that signs the envelope.
    pub source_account: String,
    /// The sequence number of the source account.
    pub sequence: i64,
    /// The operations of the envelope.
    pub operations: Vec<InvokeContractOperation>,
    /// The fee paying for the declared resources, in stroops.
    pub resource_fee_stroops: i64,
    /// The inclusion fee bid for a slot in a ledger, in stroops.
    pub inclusion_fee_stroops: i64,
    /// The ledger footprint the transaction declared.
    pub footprint: SimulateFootprint,
    /// The last ledger the envelope may be included in.
    pub valid_until_ledger: u32,
}

impl TransactionEnvelope {
    /// The canonical, base64-encoded representation of hand to the signer.
    pub fn to_base64(&self) -> Result<String, TxBuilderError> {
        let json = serde_json::to_vec(self).map_err(|err| {
            TxBuilderError::InvalidParameters(format!("could not serialise envelope: {err}"))
        })?;
        Ok(data_encoding::BASE64_NOPAD.encode(&json))
    }
}

/// A priced, simulated transaction, ready to be signed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltTransaction {
    /// The base64 envelope to sign.
    pub envelope_xdr: String,
    /// The transaction data the node produced during simulation.
    pub transaction_data: String,
    /// The chosen inclusion fee and the resource fee it is combined with.
    pub inclusion_fee: InclusionFee,
    /// The resource fee, in stroops.
    pub resource_fee_stroops: i64,
    /// The hash of the declared footprint.
    pub footprint_hash: String,
    /// Number of ledger entries read by the transaction.
    pub read_entries: usize,
    /// Number of ledger entries written by the transaction.
    pub write_entries: usize,
    /// The ledger the transaction was simulated at.
    pub simulated_ledger: u32,
    /// How often the simulation had to be repeated because the chain advanced.
    pub resimulations: u32,
}

/// Builds simulated and priced Soroban transaction envelopes.
#[derive(Clone)]
pub struct SorobanTxBuilder {
    simulator: SorobanSimulator,
    oracle: Arc<dyn FeeOracle>,
    strategy: InclusionFeeStrategy,
    pricing: ResourcePricing,
    footprint_ttl_ledgers: u32,
    max_resimulations: u32,
}

impl SorobanTxBuilder {
    /// Create a builder.
    pub fn new(
        rpc: Arc<dyn SorobanRpc>,
        oracle: Arc<dyn FeeOracle>,
        strategy: InclusionFeeStrategy,
    ) -> Self {
        Self {
            simulator: SorobanSimulator::new(rpc),
            oracle,
            strategy,
            pricing: ResourcePricing::default(),
            footprint_ttl_ledgers: DEFAULT_FOOTPRINT_TTL_LEDGERS,
            max_resimulations: 2,
        }
    }

    /// Override the price sheet used to price the declared resources.
    pub fn with_pricing(mut self, pricing: ResourcePricing) -> Self {
        self.pricing = pricing;
        self
    }

    /// Override how many ledgers a simulated footprint stays valid for.
    pub fn with_footprint_ttl(mut self, ttl_ledgers: u32) -> Self {
        self.footprint_ttl_ledgers = ttl_ledgers;
        self
    }

    /// Override how often the simulation may be repeated when the chain
    /// advances underneath it.
    pub fn with_max_resimulations(mut self, max_resimulations: u32) -> Self {
        self.max_resimulations = max_resimulations;
        self
    }

    /// The strategy this builder prices with.
    pub fn strategy(&self) -> &InclusionFeeStrategy {
        &self.strategy
    }

    /// Simulate, price and assemble a transaction.
    ///
    /// When a `budget` is given the total fee is reserved from it before the
    /// envelope is returned; a transaction that is not submitted should give
    /// the reservation back with [`DailyFeeBudget::release`].
    pub async fn build(
        &self,
        request: &TxBuildRequest,
        budget: Option<&DailyFeeBudget>,
    ) -> Result<BuiltTransaction, TxBuilderError> {
        request.validate()?;
        self.strategy.validate()?;

        let draft = self.draft_envelope(request);
        let draft_xdr = draft.to_base64()?;

        let mut resimulations = 0u32;
        let simulated = loop {
            let simulated = self.simulator.simulate(&draft_xdr).await?;
            let current = self.simulator.current_ledger().await.ok();

            let invalidated = match current {
                Some(current) => !simulated.footprint_is_valid(current, self.footprint_ttl_ledgers),
                // Without a tip we cannot prove the footprint is stale, so the
                // simulation is used as it is.
                None => false,
            };

            if !invalidated {
                break simulated;
            }

            if resimulations >= self.max_resimulations {
                return Err(TxBuilderError::Simulation(
                    SimulationError::LedgerAdvanced {
                        simulated_ledger: simulated.simulated_ledger,
                        current_ledger: current.unwrap_or_default(),
                    },
                ));
            }
            resimulations += 1;
        };

        let footprint =
            simulated
                .footprint()
                .cloned()
                .ok_or(SimulationError::FootprintUnavailable {
                    detail: "simulation did not report a ledger footprint".to_string(),
                })?;

        let resource_fee = resource_fee_for(
            &simulated.resources,
            simulated.min_resource_fee,
            &self.pricing,
        );

        let stats = self.fee_stats().await;
        let inclusion_fee = self.strategy.select(resource_fee, stats.as_ref())?;

        if let Some(budget) = budget {
            budget.try_reserve(inclusion_fee.total_stroops())?;
        }

        let envelope = self.assemble(
            request,
            &simulated,
            &footprint,
            resource_fee,
            &inclusion_fee,
        );
        let envelope_xdr = envelope.to_base64()?;

        // Only the shape of the transaction is logged: the simulation
        // envelope and the invocation arguments are never written to the log
        // at info level, because they carry the full payload of the
        // transaction.
        tracing::debug!(
            simulated_ledger = simulated.simulated_ledger,
            read_entries = footprint.read.len(),
            write_entries = footprint.write.len(),
            resource_fee_stroops = resource_fee,
            inclusion_fee_stroops = inclusion_fee.amount_stroops,
            fee_source = ?inclusion_fee.source,
            resimulations,
            "priced a transaction"
        );

        Ok(BuiltTransaction {
            envelope_xdr,
            transaction_data: simulated.transaction_data.clone(),
            inclusion_fee,
            resource_fee_stroops: resource_fee,
            footprint_hash: super::simulate::footprint_hash(&footprint),
            read_entries: footprint.read.len(),
            write_entries: footprint.write.len(),
            simulated_ledger: simulated.simulated_ledger,
            resimulations,
        })
    }

    /// The fee statistics, or `None` when the node does not serve them.
    async fn fee_stats(&self) -> Option<FeeStats> {
        match self.oracle.fee_stats().await {
            Ok(stats) => Some(stats),
            Err(err) => {
                // Missing statistics are expected on some networks: the
                // strategy then bids its floor.
                tracing::debug!(error = %err, "fee statistics unavailable, using the floor");
                None
            }
        }
    }

    /// The envelope that is sent to the simulator: the same operations, but
    /// without a footprint and without fees.
    fn draft_envelope(&self, request: &TxBuildRequest) -> TransactionEnvelope {
        TransactionEnvelope {
            source_account: request.source_account.clone(),
            sequence: request.sequence,
            operations: vec![InvokeContractOperation {
                contract_id: request.contract_id.clone(),
                function: request.function.clone(),
                args: request.args.clone(),
            }],
            resource_fee_stroops: 0,
            inclusion_fee_stroops: 0,
            footprint: SimulateFootprint::default(),
            valid_until_ledger: request
                .valid_until_ledger
                .unwrap_or_else(|| request.sequence.max(0) as u32 + 100),
        }
    }

    /// Assemble the final envelope from the simulation and the chosen fees.
    fn assemble(
        &self,
        request: &TxBuildRequest,
        simulated: &SimulatedTransaction,
        footprint: &SimulateFootprint,
        resource_fee: i64,
        inclusion_fee: &InclusionFee,
    ) -> TransactionEnvelope {
        TransactionEnvelope {
            source_account: request.source_account.clone(),
            sequence: request.sequence,
            operations: vec![InvokeContractOperation {
                contract_id: request.contract_id.clone(),
                function: request.function.clone(),
                args: request.args.clone(),
            }],
            resource_fee_stroops: resource_fee,
            inclusion_fee_stroops: inclusion_fee.amount_stroops,
            footprint: footprint.clone(),
            valid_until_ledger: request
                .valid_until_ledger
                .unwrap_or_else(|| simulated.footprint_valid_until(DEFAULT_FOOTPRINT_TTL_LEDGERS)),
        }
    }
}

/// Record what the protocol paid for a transaction.
///
/// The fee columns of the transaction record are updated and the full
/// decision is kept in `chain_tx_fees`, so an operator can audit what was
/// paid and why. Recording twice is harmless: the audit row is upserted.
pub async fn record_fee_audit(
    pool: &SqlitePool,
    tx_hash: &str,
    built: &BuiltTransaction,
) -> Result<(), sqlx::Error> {
    let source = format!("{:?}", built.inclusion_fee.source).to_lowercase();

    // The denormalised columns on the transaction row are best-effort: a
    // deployment that predates the fee columns still gets the audit row
    // below, which is the record of truth.
    if let Err(err) = sqlx::query(
        "UPDATE chain_txs
            SET inclusion_fee = ?2,
                resource_fee = ?3,
                footprint_hash = ?4,
                fee_source = ?5,
                fee_percentile = ?6
          WHERE hash = ?1",
    )
    .bind(tx_hash)
    .bind(built.inclusion_fee.amount_stroops)
    .bind(built.resource_fee_stroops)
    .bind(&built.footprint_hash)
    .bind(&source)
    .bind(built.inclusion_fee.percentile.map(|p| p as i64))
    .execute(pool)
    .await
    {
        tracing::debug!(error = %err, "could not update the fee columns of the transaction row");
    }

    sqlx::query(
        "INSERT INTO chain_tx_fees (
             tx_hash, inclusion_fee, resource_fee, footprint_hash,
             fee_source, fee_percentile, simulated_ledger
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (tx_hash) DO UPDATE SET
             inclusion_fee = excluded.inclusion_fee,
             resource_fee = excluded.resource_fee,
             footprint_hash = excluded.footprint_hash,
             fee_source = excluded.fee_source,
             fee_percentile = excluded.fee_percentile,
             simulated_ledger = excluded.simulated_ledger",
    )
    .bind(tx_hash)
    .bind(built.inclusion_fee.amount_stroops)
    .bind(built.resource_fee_stroops)
    .bind(&built.footprint_hash)
    .bind(&source)
    .bind(built.inclusion_fee.percentile.map(|p| p as i64))
    .bind(built.simulated_ledger as i64)
    .execute(pool)
    .await?;

    Ok(())
}

pub struct TxBuilder;

impl TxBuilder {
    pub async fn build_deposit_collateral(
        horizon: &HorizonClient,
        wallet: &str,
        usdc_issuer: &str,
        amount_usdc: f64,
    ) -> Result<String, TxBuilderError> {
        let report = check_wallet_readiness(horizon, wallet, usdc_issuer, amount_usdc, 0.1).await;
        if !report.ready {
            return Err(TxBuilderError::ReadinessFailed(report));
        }

        // Return simulated built transaction envelope XDR
        Ok(format!("AAAA_BUILT_TX_FOR_{wallet}_{amount_usdc}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::fees::StaticFeeOracle;
    use crate::chain::rpc::MockSorobanRpc;
    use crate::chain::simulate::SimulateResources;
    use crate::chain::types::{GetFeeStatsResponse, LedgerKey, SimulateTransactionResponse};
    use std::collections::HashMap;

    fn simulation(ledger: u32) -> SimulateTransactionResponse {
        SimulateTransactionResponse {
            transaction_data: Some("AAAA_TX_DATA".into()),
            min_resource_fee: Some("500".into()),
            results: Vec::new(),
            latest_ledger: Some(ledger),
            error: None,
            resources: Some(SimulateResources {
                instructions: 1_000,
                read_bytes: 100,
                write_bytes: 50,
                events: 1,
                footprint: Some(SimulateFootprint {
                    read: vec![LedgerKey {
                        xdr: "AAAA-READ".into(),
                    }],
                    write: vec![LedgerKey {
                        xdr: "AAAA-WRITE".into(),
                    }],
                }),
                restore_footprint_required: false,
                restore_footprint: None,
            }),
        }
    }

    fn fee_stats() -> GetFeeStatsResponse {
        GetFeeStatsResponse {
            latest_ledger: Some(1_000),
            min_fee: Some(100),
            max_fee: Some(9_000),
            p50: Some(200),
            p75: Some(400),
            p90: Some(800),
            p95: Some(1_000),
            p99: Some(2_000),
            transaction_count: Some(50),
            ..GetFeeStatsResponse::default()
        }
    }

    fn request() -> TxBuildRequest {
        TxBuildRequest {
            source_account: "GABC".into(),
            sequence: 42,
            contract_id: "CABC".into(),
            function: "deposit".into(),
            args: vec!["100".into()],
            valid_until_ledger: Some(1_010),
        }
    }

    fn builder(rpc: Arc<MockSorobanRpc>, stats: Option<GetFeeStatsResponse>) -> SorobanTxBuilder {
        let oracle: Arc<dyn FeeOracle> = match stats {
            Some(stats) => Arc::new(StaticFeeOracle::new(stats.into())),
            None => Arc::new(StaticFeeOracle::unavailable()),
        };
        SorobanTxBuilder::new(rpc, oracle, InclusionFeeStrategy::default())
    }

    #[tokio::test]
    async fn builds_an_envelope_with_the_simulated_footprint() {
        let rpc = Arc::new(MockSorobanRpc::new(1_000));
        rpc.with_simulation(Ok(simulation(1_000)));
        let builder = builder(Arc::clone(&rpc), Some(fee_stats()));

        let built = builder.build(&request(), None).await.unwrap();
        assert_eq!(built.read_entries, 1);
        assert_eq!(built.write_entries, 1);
        assert_eq!(built.footprint_hash.len(), 64);
        // 1000 instructions + 100 read + 50 write + 1 event, floored at the
        // node's minimum of 500 stroops.
        assert_eq!(built.resource_fee_stroops, 1_151);
        assert_eq!(built.inclusion_fee.amount_stroops, 800);
        assert!(!built.envelope_xdr.is_empty());
    }

    #[tokio::test]
    async fn falls_back_to_the_floor_without_fee_statistics() {
        let rpc = Arc::new(MockSorobanRpc::new(1_000));
        rpc.with_simulation(Ok(simulation(1_000)));
        let builder = builder(Arc::clone(&rpc), None);

        let built = builder.build(&request(), None).await.unwrap();
        assert_eq!(built.inclusion_fee.amount_stroops, 100);
    }

    #[tokio::test]
    async fn an_exhausted_budget_blocks_the_transaction() {
        let rpc = Arc::new(MockSorobanRpc::new(1_000));
        rpc.with_simulation(Ok(simulation(1_000)));
        let builder = builder(Arc::clone(&rpc), Some(fee_stats()));

        let budget = DailyFeeBudget::new(10);
        let err = builder.build(&request(), Some(&budget)).await.unwrap_err();
        assert!(matches!(
            err,
            TxBuilderError::FeeStrategy(
                crate::chain::fees::FeeStrategyError::BudgetExceeded { .. }
            )
        ));
    }

    #[tokio::test]
    async fn a_stale_footprint_is_simulated_again() {
        let rpc = Arc::new(MockSorobanRpc::new(1_001));
        rpc.with_simulation(Ok(simulation(998)));
        let builder = builder(Arc::clone(&rpc), Some(fee_stats()));

        // The simulation is at ledger 998, the tip is 1001 and the footprint
        // only lives for two ledgers, so the simulation has to be repeated
        // until the node stops reporting a stale footprint.
        let err = builder.build(&request(), None).await.unwrap_err();
        match err {
            TxBuilderError::Simulation(SimulationError::LedgerAdvanced {
                simulated_ledger,
                current_ledger,
            }) => {
                assert_eq!(simulated_ledger, 998);
                assert_eq!(current_ledger, 1_001);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_parameters_are_rejected() {
        let rpc = Arc::new(MockSorobanRpc::new(1_000));
        let builder = builder(rpc, None);
        let mut request = request();
        request.function = "  ".into();
        assert!(matches!(
            builder.build(&request, None).await,
            Err(TxBuilderError::InvalidParameters(_))
        ));
    }

    #[tokio::test]
    async fn a_contract_error_is_surfaced() {
        let rpc = Arc::new(MockSorobanRpc::new(1_000));
        let mut failed = simulation(1_000);
        failed.error = Some("HostError: Error(Contract, #7)".into());
        rpc.with_simulation(Ok(failed));
        let builder = builder(rpc, None);

        let err = builder.build(&request(), None).await.unwrap_err();
        assert!(matches!(
            err,
            TxBuilderError::Simulation(SimulationError::ContractError { code: Some(7), .. })
        ));
    }

    #[test]
    fn the_envelope_is_base64_of_canonical_json() {
        let envelope = TransactionEnvelope {
            source_account: "GABC".into(),
            sequence: 1,
            operations: vec![InvokeContractOperation {
                contract_id: "CABC".into(),
                function: "deposit".into(),
                args: vec![],
            }],
            resource_fee_stroops: 10,
            inclusion_fee_stroops: 20,
            footprint: SimulateFootprint::default(),
            valid_until_ledger: 100,
        };
        let encoded = envelope.to_base64().unwrap();
        let decoded = data_encoding::BASE64_NOPAD
            .decode(encoded.as_bytes())
            .unwrap();
        let parsed: HashMap<String, serde_json::Value> = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(parsed.get("resource_fee_stroops").unwrap(), 10);
        assert_eq!(parsed.get("inclusion_fee_stroops").unwrap(), 20);
    }
}
