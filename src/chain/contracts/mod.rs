use std::collections::HashMap;

pub mod options;
pub mod oracle;
pub mod vault;

pub const TOPIC_DEPOSIT: &str = "deposit";
pub const TOPIC_FILL: &str = "fill";
pub const TOPIC_SETTLE: &str = "settle";
pub const EXPECTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct ContractRegistry { network: String, ids: HashMap<&'static str, String> }

impl ContractRegistry {
    pub fn from_env(network: &str) -> Result<Self, String> {
        let suffix = network.to_ascii_uppercase().replace('-', "_");
        let mut ids = HashMap::new();
        for (name, prefix) in [("options", "ZENITH_OPTIONS_CONTRACT"), ("vault", "ZENITH_VAULT_CONTRACT"), ("oracle", "ZENITH_ORACLE_CONTRACT")] {
            let key = format!("{prefix}_{suffix}");
            let id = std::env::var(&key).map_err(|_| format!("missing {key} for enabled network {network}"))?;
            if !id.starts_with('C') || id.len() != 56 { return Err(format!("invalid contract ID in {key}")); }
            ids.insert(name, id);
        }
        Ok(Self { network: network.into(), ids })
    }

    pub fn id(&self, name: &str) -> Option<&str> { self.ids.get(name).map(String::as_str) }
    pub fn network(&self) -> &str { &self.network }
}
