use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState { Closed, Open, HalfOpen }

#[derive(Debug, Clone)]
pub struct RetryPolicy { pub max_attempts: u32, pub base_delay: Duration, pub max_delay: Duration, pub total_budget: Duration }

impl RetryPolicy {
    pub fn may_retry(method: &str) -> bool { matches!(method, "getHealth" | "getLatestLedger" | "getEvents" | "getTransaction" | "simulateTransaction") }
    pub fn delay(&self, attempt: u32, jitter_seed: u64) -> Duration {
        let cap = self.base_delay.saturating_mul(2u32.saturating_pow(attempt)).min(self.max_delay);
        Duration::from_millis(jitter_seed % (cap.as_millis() as u64 + 1))
    }
}

#[derive(Debug)]
struct Breaker { failures: u32, opened_at: Option<Instant> }

#[derive(Clone)]
pub struct RpcTransport {
    concurrency: Arc<Semaphore>,
    breaker: Arc<Mutex<Breaker>>,
    failure_threshold: u32,
    recovery_window: Duration,
    pub retry: RetryPolicy,
}

impl RpcTransport {
    pub fn new(max_in_flight: usize, failure_threshold: u32, recovery_window: Duration, retry: RetryPolicy) -> Self {
        Self { concurrency: Arc::new(Semaphore::new(max_in_flight)), breaker: Arc::new(Mutex::new(Breaker { failures: 0, opened_at: None })), failure_threshold, recovery_window, retry }
    }

    pub fn state(&self, now: Instant) -> CircuitState {
        let breaker = self.breaker.lock().unwrap();
        match breaker.opened_at { None => CircuitState::Closed, Some(opened) if now.duration_since(opened) >= self.recovery_window => CircuitState::HalfOpen, Some(_) => CircuitState::Open }
    }

    pub fn record_success(&self) { let mut breaker = self.breaker.lock().unwrap(); breaker.failures = 0; breaker.opened_at = None; }
    pub fn record_failure(&self, now: Instant) { let mut breaker = self.breaker.lock().unwrap(); breaker.failures += 1; if breaker.failures >= self.failure_threshold { breaker.opened_at = Some(now); } }
    pub async fn permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> { self.concurrency.clone().acquire_owned().await.map_err(|_| "RPC transport shut down".into()) }
}
