use serde::{Deserialize, Serialize};

pub const METRIC_NAMES: [&str; 8] = ["chain_rpc_latency_seconds", "chain_rpc_errors_total", "chain_indexer_lag_ledgers", "chain_transactions_total", "chain_confirmation_seconds", "chain_keeper_queue_depth", "chain_reconciliation_drifts", "chain_liquidations_total"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainReadiness { pub ready: bool, pub rpc: bool, pub signer: bool, pub indexer_lag_ledgers: u32, pub reason: Option<String> }

pub fn readiness(rpc_successes: u32, rpc_failures: u32, signer_reachable: bool, indexer_lag_ledgers: u32, maximum_lag: u32, failure_hysteresis: u32) -> ChainReadiness {
    let rpc = rpc_failures < failure_hysteresis || rpc_successes >= failure_hysteresis;
    let lag_ok = indexer_lag_ledgers <= maximum_lag;
    let ready = rpc && signer_reachable && lag_ok;
    let reason = (!ready).then(|| if !rpc { "RPC probe failed repeatedly" } else if !signer_reachable { "signing backend unavailable" } else { "indexer lag exceeded threshold" }.into());
    ChainReadiness { ready, rpc, signer: signer_reachable, indexer_lag_ledgers, reason }
}

pub fn correlation_id(job_id: &str, transaction_hash: Option<&str>) -> String { transaction_hash.map(|hash| format!("{job_id}:{hash}")).unwrap_or_else(|| job_id.into()) }
