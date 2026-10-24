use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus { Pending, Applied, Compensating, Compensated, Failed, NonCompensatable }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SagaStep { pub name: String, pub status: StepStatus, pub transaction_hash: Option<String>, pub ledger: Option<u32>, pub attempts: u32, pub compensatable: bool }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementSaga { pub id: String, pub idempotency_key: String, pub steps: Vec<SagaStep>, pub completed: bool }

impl SettlementSaga {
    pub fn new(id: String, idempotency_key: String) -> Self {
        let names = [("margin_lock", true), ("order_fill", true), ("settlement", false), ("collateral_release", true)];
        Self { id, idempotency_key, steps: names.into_iter().map(|(name, compensatable)| SagaStep { name: name.into(), status: StepStatus::Pending, transaction_hash: None, ledger: None, attempts: 0, compensatable }).collect(), completed: false }
    }

    pub fn mark_applied(&mut self, index: usize, hash: String, ledger: u32) -> Result<(), String> {
        if self.steps.iter().take(index).any(|step| step.status != StepStatus::Applied) { return Err("prior saga step is incomplete".into()); }
        let step = self.steps.get_mut(index).ok_or("unknown saga step")?;
        if step.status == StepStatus::Applied { return Ok(()); }
        if step.status != StepStatus::Pending { return Err("illegal saga transition".into()); }
        step.status = StepStatus::Applied; step.transaction_hash = Some(hash); step.ledger = Some(ledger); step.attempts += 1;
        self.completed = self.steps.iter().all(|step| step.status == StepStatus::Applied);
        Ok(())
    }

    pub fn begin_compensation(&mut self) { for step in self.steps.iter_mut().rev().filter(|step| step.status == StepStatus::Applied) { step.status = if step.compensatable { StepStatus::Compensating } else { StepStatus::NonCompensatable }; } }
    pub fn recovery_action(&self) -> &'static str { if self.completed { "return_original_outcome" } else if self.steps.iter().any(|step| step.transaction_hash.is_some() && step.status == StepStatus::Pending) { "query_transaction_hash" } else { "resume_next_pending_step" } }
}
