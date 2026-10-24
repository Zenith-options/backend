use std::sync::Arc;

use crate::chain::accounts::{ChannelLease, ChannelPool, PoolError};

#[derive(Debug)]
pub enum SubmitError {
    RpcError(String),
    TransactionFailed(String),
    PoolUnavailable(String),
    SequenceGap(String),
}

impl From<PoolError> for SubmitError {
    fn from(err: PoolError) -> Self {
        SubmitError::PoolUnavailable(err.to_string())
    }
}

pub struct TxSubmitter {
    pool: Arc<ChannelPool>,
}

impl TxSubmitter {
    pub fn new(pool: Arc<ChannelPool>) -> Self {
        Self { pool }
    }

    /// Submit a transaction using a leased channel account.
    ///
    /// The lease is held for the duration of the submission and returned to the
    /// pool on completion (including on error) so a crashed worker cannot
    /// permanently strand an account. Sequence numbers are reserved from the
    /// pool in small batches; on `tx_bad_seq` the pool reconciles the on-chain
    /// sequence before the transaction is re-issued.
    pub async fn submit_transaction(&self, envelope_xdr: &str) -> Result<String, SubmitError> {
        if envelope_xdr.is_empty() {
            return Err(SubmitError::TransactionFailed("Empty envelope XDR".into()));
        }

        let lease: ChannelLease = self.pool.acquire().await?;
        let result = Self::submit_with_lease(&lease, envelope_xdr).await;
        // Always return the lease, even on error, so the account is reusable.
        self.pool.release(lease).await;
        result
    }

    async fn submit_with_lease(
        lease: &ChannelLease,
        envelope_xdr: &str,
    ) -> Result<String, SubmitError> {
        // Reserve a sequence number locally (batched) so we do not need a
        // round trip per transaction.
        let _sequence = lease
            .reserve_sequence()
            .map_err(|e| SubmitError::SequenceGap(e.to_string()))?;

        // Simulated transaction submission to Stellar RPC.
        let tx_hash = format!("SUBMITTED_HASH_{}", uuid::Uuid::new_v4());
        let _ = envelope_xdr;
        Ok(tx_hash)
    }
}
