use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceLine {
    pub asset_type: String,
    pub balance: String,
    pub limit: Option<String>,
    pub asset_code: Option<String>,
    pub asset_issuer: Option<String>,
    pub buying_liabilities: Option<String>,
    pub selling_liabilities: Option<String>,
    pub is_authorized: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountResponse {
    pub id: String,
    pub sequence: String,
    pub subentry_count: u32,
    pub num_sponsoring: Option<u32>,
    pub num_sponsored: Option<u32>,
    pub balances: Vec<BalanceLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentRecord {
    pub id: String,
    pub paging_token: String,
    pub transaction_hash: String,
    pub created_at: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub amount: String,
    pub asset_type: String,
    pub asset_code: Option<String>,
    pub asset_issuer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentPage {
    pub records: Vec<PaymentRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeStats {
    pub last_ledger: String,
    pub latest_ledger_base_fee: String,
    pub ledger_capacity_usage: String,
    pub fee_charged: FeeChargedStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeChargedStats {
    pub max: String,
    pub min: String,
    pub mode: String,
    pub p10: String,
    pub p20: String,
    pub p30: String,
    pub p40: String,
    pub p50: String,
    pub p60: String,
    pub p70: String,
    pub p80: String,
    pub p90: String,
    pub p95: String,
    pub p99: String,
}

#[derive(Clone)]
pub struct HorizonClient {
    pub horizon_url: String,
    http: Client,
    cache: Arc<Mutex<HashMap<String, (AccountResponse, Instant)>>>,
    cache_ttl: Duration,
}

impl HorizonClient {
    pub fn new(horizon_url: String) -> Self {
        Self {
            horizon_url: horizon_url.trim_end_matches('/').to_string(),
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("valid Horizon HTTP client configuration"),
            cache: Arc::new(Mutex::new(HashMap::new())),
            cache_ttl: Duration::from_secs(5),
        }
    }

    pub fn insert_cached_account(&self, account: AccountResponse) {
        let mut cache = self.cache.lock().unwrap();
        cache.insert(account.id.clone(), (account, Instant::now()));
    }

    pub async fn get_account(&self, wallet: &str) -> Result<AccountResponse, HorizonError> {
        if let Some(account) = self.cached_account(wallet) {
            return Ok(account);
        }

        let response = self
            .http
            .get(self.endpoint(&["accounts", wallet])?)
            .send()
            .await
            .map_err(|e| HorizonError::NetworkError(e.to_string()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(HorizonError::AccountNotFound(wallet.to_string()));
        }
        let account = self.decode_response(response).await?;
        self.insert_cached_account(account.clone());
        Ok(account)
    }

    /// Fetch this account's payment operations, newest first, using Horizon's
    /// paging token so callers can request the next page without skipping rows.
    pub async fn get_payments(
        &self,
        wallet: &str,
        limit: u8,
        cursor: Option<&str>,
    ) -> Result<PaymentPage, HorizonError> {
        if !(1..=200).contains(&limit) {
            return Err(HorizonError::InvalidRequest(
                "payment page size must be between 1 and 200".into(),
            ));
        }

        let mut request = self
            .http
            .get(self.endpoint(&["accounts", wallet, "payments"])?)
            .query(&[("limit", limit.to_string()), ("order", "desc".to_string())]);
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        let response = request
            .send()
            .await
            .map_err(|e| HorizonError::NetworkError(e.to_string()))?;
        let payload: HorizonCollection<PaymentRecord> = self.decode_response(response).await?;
        let next_cursor = payload
            .embedded
            .records
            .last()
            .map(|record| record.paging_token.clone());
        Ok(PaymentPage {
            records: payload.embedded.records,
            next_cursor,
        })
    }

    /// Return current network fee percentiles and the latest base fee from
    /// Horizon's `/fee_stats` endpoint.
    pub async fn get_fee_stats(&self) -> Result<FeeStats, HorizonError> {
        let response = self
            .http
            .get(self.endpoint(&["fee_stats"])?)
            .send()
            .await
            .map_err(|e| HorizonError::NetworkError(e.to_string()))?;
        self.decode_response(response).await
    }

    fn cached_account(&self, wallet: &str) -> Option<AccountResponse> {
        let cache = self.cache.lock().unwrap();
        cache.get(wallet).and_then(|(account, cached_at)| {
            (cached_at.elapsed() < self.cache_ttl).then(|| account.clone())
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<reqwest::Url, HorizonError> {
        let mut url = reqwest::Url::parse(&self.horizon_url)
            .map_err(|e| HorizonError::InvalidRequest(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| HorizonError::InvalidRequest("Horizon URL cannot contain path segments".into()))?
            .extend(segments.iter().copied());
        Ok(url)
    }

    async fn decode_response<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, HorizonError> {
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(HorizonError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(HorizonError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        response
            .json()
            .await
            .map_err(|e| HorizonError::DecodeError(e.to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct HorizonCollection<T> {
    #[serde(rename = "_embedded")]
    embedded: EmbeddedRecords<T>,
}

#[derive(Debug, Deserialize)]
struct EmbeddedRecords<T> {
    records: Vec<T>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HorizonError {
    AccountNotFound(String),
    NetworkError(String),
    DecodeError(String),
    HttpStatus { status: u16, body: String },
    InvalidRequest(String),
    RateLimited,
}

impl std::fmt::Display for HorizonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccountNotFound(w) => write!(f, "Stellar account not found or unfunded: {w}"),
            Self::NetworkError(e) => write!(f, "Horizon network error: {e}"),
            Self::DecodeError(e) => write!(f, "Horizon response could not be decoded: {e}"),
            Self::HttpStatus { status, body } => {
                write!(f, "Horizon returned HTTP {status}: {body}")
            }
            Self::InvalidRequest(e) => write!(f, "Invalid Horizon request: {e}"),
            Self::RateLimited => write!(f, "Horizon rate limit exceeded"),
        }
    }
}

impl std::error::Error for HorizonError {}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};

    async fn start_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{address}")
    }

    async fn account() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "id": "GABC",
            "sequence": "123",
            "subentry_count": 2,
            "balances": [{
                "asset_type": "native",
                "balance": "42.0000000",
                "buying_liabilities": "0.0000000",
                "selling_liabilities": "1.0000000"
            }]
        }))
    }

    async fn payments() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "_embedded": {
                "records": [{
                    "id": "payment-1",
                    "paging_token": "100",
                    "transaction_hash": "tx-hash",
                    "created_at": "2026-10-21T00:00:00Z",
                    "from": "GABC",
                    "to": "GDEF",
                    "amount": "2.5",
                    "asset_type": "native"
                }]
            }
        }))
    }

    async fn fee_stats() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "last_ledger": "123",
            "latest_ledger_base_fee": "100",
            "ledger_capacity_usage": "0.2",
            "fee_charged": {
                "max": "500",
                "min": "100",
                "mode": "100",
                "p10": "100",
                "p20": "100",
                "p30": "100",
                "p40": "100",
                "p50": "100",
                "p60": "100",
                "p70": "100",
                "p80": "100",
                "p90": "200",
                "p95": "300",
                "p99": "500"
            }
        }))
    }

    #[tokio::test]
    async fn reads_account_payments_and_network_fee_statistics() {
        let app = Router::new()
            .route("/accounts/:wallet", get(account))
            .route("/accounts/:wallet/payments", get(payments))
            .route("/fee_stats", get(fee_stats));
        let client = HorizonClient::new(start_server(app).await);

        let account = client.get_account("GABC").await.unwrap();
        assert_eq!(account.sequence, "123");
        assert_eq!(account.balances[0].balance, "42.0000000");

        let payments = client.get_payments("GABC", 50, None).await.unwrap();
        assert_eq!(payments.records.len(), 1);
        assert_eq!(payments.records[0].transaction_hash, "tx-hash");
        assert_eq!(payments.next_cursor.as_deref(), Some("100"));

        let fees = client.get_fee_stats().await.unwrap();
        assert_eq!(fees.latest_ledger_base_fee, "100");
        assert_eq!(fees.fee_charged.p95, "300");
    }

    #[tokio::test]
    async fn rejects_invalid_payment_page_size() {
        let client = HorizonClient::new("http://127.0.0.1:1".into());
        assert!(matches!(
            client.get_payments("GABC", 0, None).await,
            Err(HorizonError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn maps_horizon_rate_limits() {
        let app = Router::new().route(
            "/fee_stats",
            get(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
        );
        let client = HorizonClient::new(start_server(app).await);
        assert!(matches!(
            client.get_fee_stats().await,
            Err(HorizonError::RateLimited)
        ));
    }
}
