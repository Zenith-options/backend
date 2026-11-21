use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

/// Default timeout for outbound oracle price requests.
///
/// Matches the bound described in `docs/settlement-resilience.md`: a slow or
/// hanging upstream must fail fast rather than stall the settlement worker.
pub const DEFAULT_ORACLE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum OracleError {
    #[error("oracle request timed out after {0:?}")]
    Timeout(Duration),
    #[error("oracle request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("oracle returned an unexpected status: {0}")]
    Status(reqwest::StatusCode),
    #[error("oracle returned an invalid payload: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceResponse {
    pub price: f64,
}

pub struct OracleClient {
    http: Client,
    base_url: String,
    timeout: Duration,
}

impl OracleClient {
    /// Build a client using the configured timeout, falling back to
    /// [`DEFAULT_ORACLE_TIMEOUT`] when none is supplied.
    pub fn new(base_url: impl Into<String>, timeout: Option<Duration>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.into(),
            timeout: timeout.unwrap_or(DEFAULT_ORACLE_TIMEOUT),
        }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Fetch the current price for `symbol`, bounded by the configured timeout.
    ///
    /// A timeout surfaces as [`OracleError::Timeout`] so callers can handle it
    /// without panicking.
    pub async fn fetch_price(&self, symbol: &str) -> Result<f64, OracleError> {
        let url = format!("{}/price/{}", self.base_url.trim_end_matches('/'), symbol);

        let response = self
            .http
            .get(&url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    OracleError::Timeout(self.timeout)
                } else {
                    OracleError::Request(err)
                }
            })?;

        if !response.status().is_success() {
            return Err(OracleError::Status(response.status()));
        }

        let body: PriceResponse = response
            .json()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    OracleError::Timeout(self.timeout)
                } else {
                    OracleError::Decode(err.to_string())
                }
            })?;

        Ok(body.price)
    }
}
