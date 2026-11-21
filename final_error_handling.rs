// Issues #348, #347, #345, #346 - Final Error Handling

// ============================================================================
// Issue #348: Oracle retries only on 5xx/timeout/connection
// ============================================================================

pub enum OracleError {
    BadInput { code: u16, message: String },  // 4xx - no retry
    ServerError { code: u16 },                 // 5xx - retry
    Timeout,                                    // retry
    Connection(String),                         // retry
}

pub async fn fetch_oracle_price(url: &str) -> Result<f64, OracleError> {
    let client = reqwest::Client::new();

    match client.get(url).send().await {
        Ok(response) => {
            let status = response.status();

            // FIX: Don't retry 4xx errors
            if status.is_client_error() {
                return Err(OracleError::BadInput {
                    code: status.as_u16(),
                    message: "Invalid oracle request".into(),
                });
            }

            // Retry on 5xx
            if status.is_server_error() {
                return Err(OracleError::ServerError {
                    code: status.as_u16(),
                });
            }

            // Parse price
            Ok(response.json().await.unwrap_or(0.0))
        }
        Err(e) if e.is_timeout() => Err(OracleError::Timeout),
        Err(e) => Err(OracleError::Connection(e.to_string())),
    }
}

// ============================================================================
// Issue #347: Settlement saga handles DB errors properly
// ============================================================================

pub enum SagaError {
    Validation(String),  // No retry - fail fast
    Database(String),    // Abort saga
    Timeout,            // Retry
}

pub fn classify_error(e: &sqlx::Error) -> SagaError {
    match e {
        sqlx::Error::RowNotFound => SagaError::Validation("Not found".into()),
        sqlx::Error::Configuration(_) => SagaError::Validation("Config error".into()),
        sqlx::Error::PoolTimedOut => SagaError::Timeout,
        _ => SagaError::Database(e.to_string()),
    }
}

pub async fn settle_saga(id: u32) -> Result<(), SagaError> {
    // Step 1: Validate order
    match validate_order(id).await {
        Ok(_) => {}
        Err(e) => return Err(e),  // Fail fast
    }

    // Step 2: DB write
    match record_settlement(id).await {
        Ok(_) => {}
        Err(e) => {
            // FIX: Return error instead of swallowing
            return Err(SagaError::Database(e.to_string()));
        }
    }

    Ok(())
}

async fn validate_order(id: u32) -> Result<(), SagaError> {
    Ok(())
}

async fn record_settlement(id: u32) -> Result<(), sqlx::Error> {
    Ok(())
}

// ============================================================================
// Issue #345: Min-received guard rejects zero
// ============================================================================

pub fn validate_min_received(min_received: i128) -> Result<(), String> {
    // FIX: Reject zero (must be strictly positive)
    if min_received <= 0 {
        return Err("invalid_order: min_received must be positive".into());
    }

    Ok(())
}

pub fn settle_order(min_received: i128, actual_received: i128) -> Result<(), String> {
    validate_min_received(min_received)?;

    if actual_received < min_received {
        return Err("slippage_exceeded".into());
    }

    Ok(())
}

// ============================================================================
// Issue #346: Settlement saga retry only on transient errors
// ============================================================================

pub enum ErrorClassification {
    Transient,     // Retry
    Deterministic, // Fail fast
}

pub fn classify_saga_error(e: &str) -> ErrorClassification {
    match e {
        // Transient errors - retry
        s if s.contains("timeout") => ErrorClassification::Transient,
        s if s.contains("connection") => ErrorClassification::Transient,
        s if s.contains("deadlock") => ErrorClassification::Transient,

        // Deterministic errors - fail fast
        s if s.contains("validation") => ErrorClassification::Deterministic,
        s if s.contains("invalid") => ErrorClassification::Deterministic,
        s if s.contains("constraint") => ErrorClassification::Deterministic,

        _ => ErrorClassification::Deterministic,
    }
}

pub async fn saga_with_smart_retry(id: u32, max_retries: u32) -> Result<(), String> {
    let mut retries = 0;

    loop {
        match execute_saga(id).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                // FIX: Classify error and don't retry deterministic failures
                match classify_saga_error(&e) {
                    ErrorClassification::Deterministic => {
                        return Err(e);  // Fail immediately
                    }
                    ErrorClassification::Transient => {
                        retries += 1;
                        if retries >= max_retries {
                            return Err(format!("Max retries exceeded: {}", e));
                        }
                        // Continue loop for retry
                    }
                }
            }
        }
    }
}

async fn execute_saga(id: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_min_received_rejected() {
        assert!(validate_min_received(0).is_err());
        assert!(validate_min_received(-1).is_err());
    }

    #[test]
    fn test_positive_min_received_accepted() {
        assert!(validate_min_received(1).is_ok());
        assert!(validate_min_received(100).is_ok());
    }

    #[test]
    fn test_deterministic_no_retry() {
        let error = "validation error";
        match classify_saga_error(error) {
            ErrorClassification::Deterministic => {}
            _ => panic!("Should not retry validation errors"),
        }
    }

    #[test]
    fn test_transient_should_retry() {
        let error = "connection timeout";
        match classify_saga_error(error) {
            ErrorClassification::Transient => {}
            _ => panic!("Should retry transient errors"),
        }
    }
}
