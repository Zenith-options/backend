use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MINIMUM_BUCKET_WALLETS: usize = 10;
pub const PUBLIC_DATASETS: [&str; 10] = [
    "daily-volume", "open-interest", "pnl-distribution", "strategy-popularity",
    "settlements", "insurance-fund", "governance-participation", "treasury",
    "fees", "defillama",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateBucket {
    pub label: String,
    pub wallet_count: usize,
    pub value_minor: i128,
}

pub fn suppress_small_buckets(buckets: impl IntoIterator<Item = AggregateBucket>) -> Vec<AggregateBucket> {
    buckets.into_iter().filter(|bucket| bucket.wallet_count >= MINIMUM_BUCKET_WALLETS).collect()
}

pub fn cache_headers(payload: &[u8]) -> [(String, String); 3] {
    [
        ("cache-control".into(), "public, max-age=300, stale-while-revalidate=60".into()),
        ("etag".into(), format!("\"{:x}\"", Sha256::digest(payload))),
        ("deprecation".into(), "false".into()),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefiLlamaSnapshot { pub timestamp: i64, pub tvl: f64, pub daily_volume: f64 }
