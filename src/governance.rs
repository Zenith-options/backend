use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState { Pending, Active, Succeeded, Defeated, Queued, Executed, Cancelled }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceAction {
    pub contract: String,
    pub function: String,
    pub arguments: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalProjection {
    pub id: String,
    pub state: ProposalState,
    pub starts_at: i64,
    pub ends_at: i64,
    pub for_power: i128,
    pub against_power: i128,
    pub quorum_power: i128,
    pub actions: Vec<GovernanceAction>,
}

impl ProposalProjection {
    pub fn quorum_progress(&self) -> f64 {
        if self.quorum_power <= 0 { return 1.0; }
        ((self.for_power + self.against_power) as f64 / self.quorum_power as f64).min(1.0)
    }

    pub fn reconcile_tally(&self, contract_for: i128, contract_against: i128) -> Result<(), String> {
        if self.for_power == contract_for && self.against_power == contract_against { Ok(()) } else { Err(format!("proposal {} tally drift", self.id)) }
    }
}

pub fn decode_action(contract: String, function: String, arguments: Vec<String>) -> GovernanceAction {
    let description = match function.as_str() {
        "set_parameter" if arguments.len() >= 2 => format!("set protocol parameter {} to {}", arguments[0], arguments[1]),
        "upgrade" => "upgrade target contract code".into(),
        _ => format!("call {function}({})", arguments.join(", ")),
    };
    GovernanceAction { contract, function, arguments, description }
}
