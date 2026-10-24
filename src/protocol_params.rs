use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterVersion {
    pub key: String,
    pub value: i128,
    pub effective_from: i64,
    pub source_proposal_id: Option<String>,
    pub set_by: String,
}

#[derive(Debug, Clone, Copy)]
pub struct SafetyBounds { pub minimum: i128, pub maximum: i128, pub higher_is_tighter: bool }

impl SafetyBounds {
    pub fn validate(self, value: i128) -> Result<(), String> {
        if (self.minimum..=self.maximum).contains(&value) { Ok(()) } else { Err(format!("parameter outside safety bounds {}..={}", self.minimum, self.maximum)) }
    }

    pub fn validate_emergency(self, current: i128, proposed: i128) -> Result<(), String> {
        self.validate(proposed)?;
        let tighter = if self.higher_is_tighter { proposed >= current } else { proposed <= current };
        tighter.then_some(()).ok_or_else(|| "emergency multisig may only tighten risk".into())
    }
}

pub fn effective_at<'a>(versions: &'a [ParameterVersion], key: &str, timestamp: i64) -> Option<&'a ParameterVersion> {
    versions.iter().filter(|version| version.key == key && version.effective_from <= timestamp).max_by_key(|version| version.effective_from)
}
