use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ChainConfig {
    pub rpc_url: String,
    pub network_passphrase: String,
    pub friendbot_enabled: bool,
    pub request_timeout: Duration,
    pub contract_ids: HashMap<String, String>,
}

impl ChainConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let rpc_url = match std::env::var("SOROBAN_RPC_URL") { Ok(value) if !value.trim().is_empty() => value, _ => return Ok(None) };
        if !(rpc_url.starts_with("https://") || rpc_url.starts_with("http://localhost")) { return Err("SOROBAN_RPC_URL must use HTTPS outside localhost".into()); }
        let network_passphrase = std::env::var("STELLAR_NETWORK_PASSPHRASE").map_err(|_| "STELLAR_NETWORK_PASSPHRASE is required when chain mode is configured")?;
        let timeout_ms = std::env::var("SOROBAN_RPC_TIMEOUT_MS").ok().and_then(|value| value.parse().ok()).unwrap_or(5_000);
        Ok(Some(Self { rpc_url, network_passphrase, friendbot_enabled: std::env::var("STELLAR_FRIENDBOT_ENABLED").as_deref() == Ok("true"), request_timeout: Duration::from_millis(timeout_ms), contract_ids: HashMap::new() }))
    }
}
