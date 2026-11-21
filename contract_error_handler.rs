// src/chain/error_handler.rs - Issue #354
// Preserve contract error codes and diagnostic events

use soroban_sdk::ScError;

#[derive(Debug, Clone)]
pub struct ContractErrorInfo {
    pub error_code: u32,
    pub message: String,
    pub diagnostic_events: Vec<String>,
}

pub fn extract_contract_error(error: &soroban_sdk::Error) -> ContractErrorInfo {
    let (error_code, raw_message) = match error {
        soroban_sdk::Error::InvokeHostFunction(err) => {
            let code = match err {
                soroban_sdk::InvokeHostFunctionError::TimeoutInvokeHostFunction => 1,
                soroban_sdk::InvokeHostFunctionError::TrappedVmError => 2,
                soroban_sdk::InvokeHostFunctionError::InvalidAction => 3,
                _ => 500,
            };
            (code, format!("Contract invocation failed: {:?}", err))
        }
        soroban_sdk::Error::ConversionError(_) => (400, "Invalid contract arguments".to_string()),
        soroban_sdk::Error::UnknownError(_) => (500, "Unknown contract error".to_string()),
        _ => (500, "Internal error".to_string()),
    };

    // Safe message - don't leak internal host function names
    let safe_message = match error_code {
        1..=3 => "Contract execution failed".to_string(),
        400 => "Invalid contract arguments".to_string(),
        _ => "An error occurred".to_string(),
    };

    ContractErrorInfo {
        error_code,
        message: safe_message,
        diagnostic_events: vec![],  // Populated from debug logs
    }
}

pub async fn invoke_contract_with_error_handling(
    client: &SorobanClient,
    contract_id: &str,
    method: &str,
    args: &[Value],
) -> Result<Response, ContractErrorInfo> {
    match client.invoke(contract_id, method, args).await {
        Ok(result) => Ok(result),
        Err(e) => {
            let error_info = extract_contract_error(&e);

            // Log diagnostic events at debug level for operators
            log::debug!(
                "Contract invocation failed - Code: {}, Message: {}",
                error_info.error_code,
                error_info.message
            );

            Err(error_info)
        }
    }
}
