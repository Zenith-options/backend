use std::time::Duration;

use metrics::counter;
use tracing::debug;

/// Reason a retry was triggered. Used as a metric label and in debug logs.
#[derive(Debug, Clone, Copy)]
pub enum RetryReason {
    Timeout,
    Transport,
    ServerError,
}

impl RetryReason {
    fn as_label(self) -> &'static str {
        match self {
            RetryReason::Timeout => "timeout",
            RetryReason::Transport => "transport",
            RetryReason::ServerError => "server_error",
        }
    }
}

/// Emit observability for a single retry attempt.
///
/// Logs at debug level (bounded volume) and increments the retry counter
/// labelled by reason. Does not alter retry policy.
fn record_retry(attempt: u32, reason: RetryReason) {
    debug!(
        attempt = attempt,
        reason = reason.as_label(),
        "rpc request retry"
    );
    counter!(
        "rpc_client_retries_total",
        "reason" => reason.as_label()
    )
    .increment(1);
}

pub struct RpcClient {
    // ... existing fields unchanged ...
    max_retries: u32,
    retry_backoff: Duration,
}

impl RpcClient {
    /// Execute a request, retrying on failure per the existing policy.
    async fn request_with_retries<T, F, Fut>(&self, mut op: F) -> Result<T, RpcError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, RpcError>>,
    {
        let mut attempt: u32 = 0;
        loop {
            match op().await {
                Ok(value) => return Ok(value),
                Err(err) => {
                    let reason = err.retry_reason();
                    if attempt >= self.max_retries || !err.is_retryable() {
                        return Err(err);
                    }
                    attempt += 1;
                    record_retry(attempt, reason);
                    tokio::time::sleep(self.retry_backoff).await;
                }
            }
        }
    }
}
