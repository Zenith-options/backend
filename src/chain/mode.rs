use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChainMode { Paper, Shadow, Live }

impl ChainMode {
    pub fn parse(value: &str) -> Result<Self, String> { match value { "paper" => Ok(Self::Paper), "shadow" => Ok(Self::Shadow), "live" => Ok(Self::Live), _ => Err("CHAIN_MODE must be paper, shadow, or live".into()) } }
    pub fn rpc_reads(self) -> bool { !matches!(self, Self::Paper) }
    pub fn chain_writes(self) -> bool { matches!(self, Self::Live) }
    pub fn paper_writes(self) -> bool { matches!(self, Self::Paper) }
    pub fn on_chain_degraded(self) -> Result<(), &'static str> { match self { Self::Paper => Ok(()), Self::Shadow => Ok(()), Self::Live => Err("live chain mode unavailable; operation queued or rejected") } }
}
