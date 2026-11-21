// Issues #350, #352, #351, #349 - Logging and Error Handling

// ============================================================================
// Issue #350: Indexer backfill failure tracking
// ============================================================================

use sqlx::SqlitePool;

pub struct BackfillFailure {
    pub block_number: u64,
    pub error_message: String,
    pub timestamp: i64,
}

pub async fn log_backfill_failure(
    pool: &SqlitePool,
    block_number: u64,
    error: &str,
) -> Result<(), sqlx::Error> {
    // Store failure in persistent table
    sqlx::query(
        "INSERT INTO backfill_failures (block_number, error_message, timestamp) VALUES (?, ?, ?)"
    )
    .bind(block_number)
    .bind(error)
    .bind(chrono::Utc::now().timestamp())
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn backfill_with_error_tracking(
    pool: &SqlitePool,
    start_block: u64,
    end_block: u64,
) -> Result<BackfillStats, Box<dyn std::error::Error>> {
    let mut processed = 0;
    let mut failed = 0;

    for block_num in start_block..=end_block {
        match fetch_and_index_block(block_num).await {
            Ok(_) => processed += 1,
            Err(e) => {
                // FIX: Record failure instead of silently skipping
                log_backfill_failure(pool, block_num, &e.to_string()).await?;
                failed += 1;
                log::warn!("Block {} parse error: {}", block_num, e);
            }
        }
    }

    // FIX: Return non-zero exit if failures occurred
    if failed > 0 {
        return Err(format!("Backfill completed with {} failures", failed).into());
    }

    Ok(BackfillStats { processed, failed })
}

pub struct BackfillStats {
    pub processed: u64,
    pub failed: u64,
}

async fn fetch_and_index_block(block_num: u64) -> Result<(), Box<dyn std::error::Error>> {
    // Block processing logic
    Ok(())
}

// ============================================================================
// Issue #352: Pool timeout with clear messaging
// ============================================================================

pub enum DbError {
    PoolExhausted { size: u32, wait_ms: u64 },
    QueryTimeout,
    Other(String),
}

pub fn map_sqlx_error(e: sqlx::Error) -> (DbError, u16) {
    match e {
        sqlx::Error::PoolTimedOut => {
            // FIX: Distinct error for pool timeout
            log::warn!("DB pool exhausted: size limit reached, wait exceeded");
            (
                DbError::PoolExhausted { size: 10, wait_ms: 5000 },
                503,  // Service Unavailable
            )
        }
        sqlx::Error::PoolClosed => {
            (DbError::Other("Connection pool closed".into()), 503)
        }
        _ => (DbError::Other(e.to_string()), 500),
    }
}

// ============================================================================
// Issue #351: Malformed JSON returns 400
// ============================================================================

pub struct ApiError {
    pub code: String,
    pub message: String,
    pub status: u16,
}

pub fn handle_json_error(e: serde_json::Error) -> (ApiError, u16) {
    // FIX: Return 400 for malformed JSON
    log::debug!("Malformed JSON: {}", e);

    (
        ApiError {
            code: "invalid_request".into(),
            message: format!("Invalid JSON: {}", e),
            status: 400,
        },
        400,
    )
}

pub fn json_error_handler(e: serde_json::Error) -> actix_web::HttpResponse {
    let (api_err, status) = handle_json_error(e);

    actix_web::HttpResponse::BadRequest().json(serde_json::json!({
        "error": {
            "code": api_err.code,
            "message": api_err.message
        }
    }))
}

// ============================================================================
// Issue #349: Webhook failure logging with details
// ============================================================================

pub struct WebhookLog {
    pub url: String,  // Redacted if contains token
    pub status: u16,
    pub response_body: String,  // Max 512 bytes
}

pub fn redact_url(url: &str) -> String {
    // Redact tokens from URL
    if url.contains("token=") {
        url.split("token=").next().unwrap_or("").to_string() + "token=[REDACTED]"
    } else {
        url.to_string()
    }
}

pub async fn deliver_webhook_with_logging(
    url: &str,
    payload: &str,
) -> Result<(), WebhookLog> {
    let client = reqwest::Client::new();

    match client.post(url).json(&serde_json::json!({})).send().await {
        Ok(response) => {
            if response.status().is_success() {
                Ok(())
            } else {
                // FIX: Log with URL, status, and response body
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "[unreadable]".into());

                let truncated = if body.len() > 512 {
                    format!("{}...[truncated]", &body[..512])
                } else {
                    body
                };

                log::warn!(
                    "Webhook failed: url={}, status={}, response={}",
                    redact_url(url),
                    response.status(),
                    truncated
                );

                Err(WebhookLog {
                    url: redact_url(url),
                    status: response.status().as_u16(),
                    response_body: truncated,
                })
            }
        }
        Err(e) => {
            log::error!("Webhook delivery error: url={}, error={}", redact_url(url), e);
            Err(WebhookLog {
                url: redact_url(url),
                status: 0,
                response_body: e.to_string(),
            })
        }
    }
}
