use std::env;

/// Default number of blocks written per batch during backfill.
const DEFAULT_BACKFILL_BATCH_SIZE: usize = 500;

#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub database_url: String,
    pub rpc_url: String,
    pub start_block: u64,
    pub backfill_batch_size: usize,
}

impl IndexerConfig {
    pub fn from_env() -> Self {
        let backfill_batch_size = env::var("INDEXER_BACKFILL_BATCH_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_BACKFILL_BATCH_SIZE);

        Self {
            database_url: env::var("DATABASE_URL").expect("DATABASE_URL must be set"),
            rpc_url: env::var("RPC_URL").expect("RPC_URL must be set"),
            start_block: env::var("START_BLOCK")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0),
            backfill_batch_size,
        }
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            rpc_url: String::new(),
            start_block: 0,
            backfill_batch_size: DEFAULT_BACKFILL_BATCH_SIZE,
        }
    }
}
