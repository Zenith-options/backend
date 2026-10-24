//! Durable, bounded settlement job queue with priority classes and fair
//! scheduling.
//!
//! Every piece of settlement work is a row in `settlement_jobs`, so nothing
//! is lost on a restart. Work falls into three priority classes:
//!
//! | class    | kinds                            | why                              |
//! |----------|----------------------------------|----------------------------------|
//! | `urgent` | `liquidation`, `expiry`          | delay costs money / breaks SLAs  |
//! | `normal` | `settlement`                     | user-initiated settlement        |
//! | `bulk`   | `backfill`, `oracle_republish`   | catch-up work, can wait          |
//!
//! Three mechanisms keep urgent work from being starved by bulk work:
//!
//! 1. **Fair scheduling.** Workers pick the next class with smooth weighted
//!    round-robin ([`FairScheduler`]). With default weights 8/3/1 urgent
//!    work gets at least 2/3 of claims when everything is backed up, but bulk
//!    still gets its 1/12 share, so it is slowed down, never frozen.
//! 2. **Lease caps.** Each class may hold at most `max_leased` jobs at once.
//!    Bulk jobs can't occupy every worker, so a worker is always free to pick
//!    up a liquidation.
//! 3. **Bounded queue + load shedding.** Each class has a hard capacity. On top
//!    of that, once the total backlog crosses `shed_bulk_at`, new bulk work is
//!    rejected, and past `shed_normal_at` normal work is rejected too. Urgent
//!    work is only ever rejected at its own hard capacity. Callers get a typed
//!    [`EnqueueError`] they can turn into a 503 / retry-later.
//!
//! Workers claim jobs with a lease. A worker that dies leaves its lease to
//! expire, after which the job is requeued (or dead-lettered once it has used
//! up `max_attempts`). Failed jobs retry with exponential backoff.
//!
//! All timestamps are Unix milliseconds and passed in by the caller, so the
//! queue is deterministic under test.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::sync::Mutex;

// ─── Job kinds and priority classes ──────────────────────────────────────────

/// What a settlement job does. Determines its [`PriorityClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Liquidation,
    Expiry,
    Settlement,
    Backfill,
    OracleRepublish,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Liquidation => "liquidation",
            JobKind::Expiry => "expiry",
            JobKind::Settlement => "settlement",
            JobKind::Backfill => "backfill",
            JobKind::OracleRepublish => "oracle_republish",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "liquidation" => JobKind::Liquidation,
            "expiry" => JobKind::Expiry,
            "settlement" => JobKind::Settlement,
            "backfill" => JobKind::Backfill,
            "oracle_republish" => JobKind::OracleRepublish,
            _ => return None,
        })
    }

    pub fn class(self) -> PriorityClass {
        match self {
            JobKind::Liquidation | JobKind::Expiry => PriorityClass::Urgent,
            JobKind::Settlement => PriorityClass::Normal,
            JobKind::Backfill | JobKind::OracleRepublish => PriorityClass::Bulk,
        }
    }
}

/// Priority class, ordered from most to least urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityClass {
    Urgent,
    Normal,
    Bulk,
}

impl PriorityClass {
    pub const ALL: [PriorityClass; 3] = [
        PriorityClass::Urgent,
        PriorityClass::Normal,
        PriorityClass::Bulk,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PriorityClass::Urgent => "urgent",
            PriorityClass::Normal => "normal",
            PriorityClass::Bulk => "bulk",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "urgent" => PriorityClass::Urgent,
            "normal" => PriorityClass::Normal,
            "bulk" => PriorityClass::Bulk,
            _ => return None,
        })
    }
}

// ─── Configuration ───────────────────────────────────────────────────────────

/// Per-class limits and scheduling weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassPolicy {
    /// Relative share of claims when several classes have work ready.
    pub weight: u32,
    /// Hard cap on queued + leased jobs in this class.
    pub capacity: u64,
    /// Cap on jobs of this class leased at the same time.
    pub max_leased: u64,
}

#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Indexed by `PriorityClass as usize`.
    pub classes: [ClassPolicy; 3],
    /// Total backlog (all classes) at which new bulk jobs are rejected.
    pub shed_bulk_at: u64,
    /// Total backlog at which new normal jobs are rejected.
    pub shed_normal_at: u64,
    pub lease_ms: i64,
    pub max_attempts: i64,
    pub base_backoff_ms: i64,
    pub max_backoff_ms: i64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            classes: [
                ClassPolicy {
                    weight: 8,
                    capacity: 50_000,
                    max_leased: 64,
                },
                ClassPolicy {
                    weight: 3,
                    capacity: 20_000,
                    max_leased: 32,
                },
                ClassPolicy {
                    weight: 1,
                    capacity: 10_000,
                    max_leased: 4,
                },
            ],
            shed_bulk_at: 20_000,
            shed_normal_at: 40_000,
            lease_ms: 60_000,
            max_attempts: 5,
            base_backoff_ms: 1_000,
            max_backoff_ms: 5 * 60_000,
        }
    }
}

impl QueueConfig {
    pub fn policy(&self, class: PriorityClass) -> ClassPolicy {
        self.classes[class.index()]
    }

    /// Total backlog at which `class` starts being shed, if ever.
    fn shed_threshold(&self, class: PriorityClass) -> Option<u64> {
        match class {
            PriorityClass::Urgent => None,
            PriorityClass::Normal => Some(self.shed_normal_at),
            PriorityClass::Bulk => Some(self.shed_bulk_at),
        }
    }

    /// Delay before retry number `attempts` (1-based), doubling each time.
    pub fn backoff_ms(&self, attempts: i64) -> i64 {
        let exp = (attempts.max(1) - 1).min(30) as u32;
        self.base_backoff_ms
            .saturating_mul(1i64 << exp)
            .min(self.max_backoff_ms)
    }
}

// ─── Fair scheduler ──────────────────────────────────────────────────────────

/// Smooth weighted round-robin over priority classes (the algorithm nginx
/// uses for upstream weights). Given which classes are eligible right now it
/// returns the one to serve next. Over any window where all classes stay
/// eligible, each class gets exactly its weight's share of picks, spread out
/// evenly rather than in bursts. Ties go to the more urgent class.
#[derive(Debug, Clone)]
pub struct FairScheduler {
    weights: [i64; 3],
    current: [i64; 3],
}

impl FairScheduler {
    pub fn new(config: &QueueConfig) -> Self {
        Self {
            weights: config.classes.map(|c| c.weight.max(1) as i64),
            current: [0; 3],
        }
    }

    pub fn pick(&mut self, eligible: [bool; 3]) -> Option<PriorityClass> {
        let total: i64 = (0..3)
            .filter(|&i| eligible[i])
            .map(|i| self.weights[i])
            .sum();
        if total == 0 {
            return None;
        }
        let mut best: Option<usize> = None;
        for (i, _) in eligible.iter().enumerate().filter(|(_, e)| **e) {
            self.current[i] += self.weights[i];
            if best.is_none_or(|b| self.current[i] > self.current[b]) {
                best = Some(i);
            }
        }
        let b = best?;
        self.current[b] -= total;
        Some(PriorityClass::ALL[b])
    }
}

// ─── Queue ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum EnqueueError {
    /// The class is at its hard capacity.
    QueueFull {
        class: PriorityClass,
        depth: u64,
        capacity: u64,
    },
    /// The overall backlog is high enough that this class is being shed.
    Shed {
        class: PriorityClass,
        total_depth: u64,
        threshold: u64,
    },
    Db(sqlx::Error),
}

impl std::fmt::Display for EnqueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnqueueError::QueueFull {
                class,
                depth,
                capacity,
            } => write!(f, "{} queue full ({depth}/{capacity})", class.as_str()),
            EnqueueError::Shed {
                class,
                total_depth,
                threshold,
            } => write!(
                f,
                "shedding {} work: backlog {total_depth} >= {threshold}",
                class.as_str()
            ),
            EnqueueError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for EnqueueError {}

impl From<sqlx::Error> for EnqueueError {
    fn from(e: sqlx::Error) -> Self {
        EnqueueError::Db(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Enqueued(String),
    /// An active job with the same dedupe key already exists; its id.
    Duplicate(String),
}

impl EnqueueOutcome {
    pub fn id(&self) -> &str {
        match self {
            EnqueueOutcome::Enqueued(id) | EnqueueOutcome::Duplicate(id) => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    pub id: String,
    pub kind: JobKind,
    pub class: PriorityClass,
    pub payload: serde_json::Value,
    pub dedupe_key: Option<String>,
    /// Attempt number of this claim, starting at 1.
    pub attempt: i64,
    pub lease_expires_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// Requeued; becomes claimable again at this time.
    Retrying { available_at_ms: i64 },
    /// Out of attempts; parked in `dead` for an operator.
    Dead,
    /// The lease was no longer ours (it expired and was reclaimed).
    LeaseLost,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ClassStats {
    pub queued: u64,
    pub leased: u64,
    pub dead: u64,
    /// Age of the oldest ready job, the best single signal of starvation.
    pub oldest_ready_age_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct QueueStats {
    pub urgent: ClassStats,
    pub normal: ClassStats,
    pub bulk: ClassStats,
}

impl QueueStats {
    pub fn class(&self, class: PriorityClass) -> &ClassStats {
        match class {
            PriorityClass::Urgent => &self.urgent,
            PriorityClass::Normal => &self.normal,
            PriorityClass::Bulk => &self.bulk,
        }
    }

    fn class_mut(&mut self, class: PriorityClass) -> &mut ClassStats {
        match class {
            PriorityClass::Urgent => &mut self.urgent,
            PriorityClass::Normal => &mut self.normal,
            PriorityClass::Bulk => &mut self.bulk,
        }
    }

    pub fn total_active(&self) -> u64 {
        PriorityClass::ALL
            .iter()
            .map(|c| self.class(*c).queued + self.class(*c).leased)
            .sum()
    }
}

pub struct SettlementQueue {
    pool: SqlitePool,
    config: QueueConfig,
    scheduler: Mutex<FairScheduler>,
}

impl SettlementQueue {
    pub fn new(pool: SqlitePool, config: QueueConfig) -> Self {
        let scheduler = Mutex::new(FairScheduler::new(&config));
        Self {
            pool,
            config,
            scheduler,
        }
    }

    pub fn config(&self) -> &QueueConfig {
        &self.config
    }

    /// Adds a job. Capacity, load shedding and deduplication are checked in
    /// the same statement as the insert, so concurrent producers can't
    /// overshoot the bound.
    pub async fn enqueue(
        &self,
        kind: JobKind,
        payload: &serde_json::Value,
        dedupe_key: Option<&str>,
        now_ms: i64,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        let class = kind.class();
        let policy = self.config.policy(class);
        // i64::MAX means "no shed threshold" for urgent work.
        let shed_at = self
            .config
            .shed_threshold(class)
            .map_or(i64::MAX, |t| t as i64);
        let id = uuid::Uuid::new_v4().to_string();

        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO settlement_jobs
                 (id, kind, class, payload, dedupe_key, max_attempts,
                  available_at_ms, created_at_ms, updated_at_ms)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?7
             WHERE (SELECT COUNT(*) FROM settlement_jobs
                     WHERE class = ?3 AND status IN ('queued', 'leased')) < ?8
               AND (SELECT COUNT(*) FROM settlement_jobs
                     WHERE status IN ('queued', 'leased')) < ?9",
        )
        .bind(&id)
        .bind(kind.as_str())
        .bind(class.as_str())
        .bind(payload.to_string())
        .bind(dedupe_key)
        .bind(self.config.max_attempts)
        .bind(now_ms)
        .bind(policy.capacity as i64)
        .bind(shed_at)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if inserted == 1 {
            return Ok(EnqueueOutcome::Enqueued(id));
        }

        // Not inserted: either a duplicate or a capacity / shedding rejection.
        if let Some(key) = dedupe_key {
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT id FROM settlement_jobs
                  WHERE dedupe_key = ? AND status IN ('queued', 'leased')",
            )
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(existing) = existing {
                return Ok(EnqueueOutcome::Duplicate(existing));
            }
        }

        let stats = self.stats(now_ms).await?;
        let c = stats.class(class);
        let depth = c.queued + c.leased;
        if depth >= policy.capacity {
            return Err(EnqueueError::QueueFull {
                class,
                depth,
                capacity: policy.capacity,
            });
        }
        Err(EnqueueError::Shed {
            class,
            total_depth: stats.total_active(),
            threshold: shed_at as u64,
        })
    }

    /// Claims the next job for `worker`, or `None` if nothing is eligible.
    /// Expired leases are reclaimed first so a crashed worker's job is never
    /// stuck.
    pub async fn claim(
        &self,
        worker: &str,
        now_ms: i64,
    ) -> Result<Option<ClaimedJob>, sqlx::Error> {
        self.reclaim_expired(now_ms).await?;

        let rows = sqlx::query(
            "SELECT class,
                    SUM(CASE WHEN status = 'queued' AND available_at_ms <= ?1 THEN 1 ELSE 0 END) AS ready,
                    SUM(CASE WHEN status = 'leased' THEN 1 ELSE 0 END) AS leased
               FROM settlement_jobs
              WHERE status IN ('queued', 'leased')
              GROUP BY class",
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await?;

        let mut eligible = [false; 3];
        for row in rows {
            let Some(class) = PriorityClass::parse(row.get::<&str, _>("class")) else {
                continue;
            };
            let ready: i64 = row.get("ready");
            let leased: i64 = row.get("leased");
            eligible[class.index()] =
                ready > 0 && (leased as u64) < self.config.policy(class).max_leased;
        }

        // Try classes in scheduler order; if another worker grabbed the last
        // ready job of the picked class, fall through to the next one.
        for _ in 0..3 {
            let Some(class) = self.scheduler.lock().unwrap().pick(eligible) else {
                return Ok(None);
            };
            if let Some(job) = self.claim_from_class(class, worker, now_ms).await? {
                return Ok(Some(job));
            }
            eligible[class.index()] = false;
        }
        Ok(None)
    }

    async fn claim_from_class(
        &self,
        class: PriorityClass,
        worker: &str,
        now_ms: i64,
    ) -> Result<Option<ClaimedJob>, sqlx::Error> {
        let lease_expires_ms = now_ms + self.config.lease_ms;
        let row = sqlx::query(
            "UPDATE settlement_jobs
                SET status = 'leased', lease_owner = ?1, lease_expires_ms = ?2,
                    attempts = attempts + 1, updated_at_ms = ?3
              WHERE seq = (SELECT seq FROM settlement_jobs
                            WHERE class = ?4 AND status = 'queued' AND available_at_ms <= ?3
                            ORDER BY seq LIMIT 1)
                AND status = 'queued'
          RETURNING id, kind, payload, dedupe_key, attempts",
        )
        .bind(worker)
        .bind(lease_expires_ms)
        .bind(now_ms)
        .bind(class.as_str())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| ClaimedJob {
            id: row.get("id"),
            kind: JobKind::parse(row.get::<&str, _>("kind")).expect("kind constrained by CHECK"),
            class,
            payload: serde_json::from_str(row.get::<&str, _>("payload"))
                .unwrap_or(serde_json::Value::Null),
            dedupe_key: row.get("dedupe_key"),
            attempt: row.get("attempts"),
            lease_expires_ms,
        }))
    }

    /// Extends a lease for a long-running job. Returns false if the lease was
    /// lost.
    pub async fn heartbeat(
        &self,
        id: &str,
        worker: &str,
        now_ms: i64,
    ) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE settlement_jobs SET lease_expires_ms = ?1, updated_at_ms = ?2
              WHERE id = ?3 AND status = 'leased' AND lease_owner = ?4 AND lease_expires_ms > ?2",
        )
        .bind(now_ms + self.config.lease_ms)
        .bind(now_ms)
        .bind(id)
        .bind(worker)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Marks a job done. Returns false if the lease was lost (another worker
    /// may be running it), in which case the caller's work must be idempotent.
    pub async fn complete(&self, id: &str, worker: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE settlement_jobs
                SET status = 'done', lease_owner = NULL, lease_expires_ms = NULL, updated_at_ms = ?1
              WHERE id = ?2 AND status = 'leased' AND lease_owner = ?3",
        )
        .bind(now_ms)
        .bind(id)
        .bind(worker)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Records a failure: retries with backoff or dead-letters the job.
    pub async fn fail(
        &self,
        id: &str,
        worker: &str,
        error: &str,
        now_ms: i64,
    ) -> Result<FailOutcome, sqlx::Error> {
        let attempts: Option<(i64, i64)> = sqlx::query_as(
            "SELECT attempts, max_attempts FROM settlement_jobs
              WHERE id = ? AND status = 'leased' AND lease_owner = ?",
        )
        .bind(id)
        .bind(worker)
        .fetch_optional(&self.pool)
        .await?;
        let Some((attempts, max_attempts)) = attempts else {
            return Ok(FailOutcome::LeaseLost);
        };

        let (status, available_at_ms, outcome) = if attempts >= max_attempts {
            ("dead", now_ms, FailOutcome::Dead)
        } else {
            let at = now_ms + self.config.backoff_ms(attempts);
            (
                "queued",
                at,
                FailOutcome::Retrying {
                    available_at_ms: at,
                },
            )
        };

        let n = sqlx::query(
            "UPDATE settlement_jobs
                SET status = ?1, available_at_ms = ?2, last_error = ?3,
                    lease_owner = NULL, lease_expires_ms = NULL, updated_at_ms = ?4
              WHERE id = ?5 AND status = 'leased' AND lease_owner = ?6",
        )
        .bind(status)
        .bind(available_at_ms)
        .bind(error)
        .bind(now_ms)
        .bind(id)
        .bind(worker)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(if n == 1 {
            outcome
        } else {
            FailOutcome::LeaseLost
        })
    }

    /// Requeues jobs whose lease ran out, dead-lettering the ones that have
    /// used every attempt. Returns how many leases were reclaimed.
    pub async fn reclaim_expired(&self, now_ms: i64) -> Result<u64, sqlx::Error> {
        let dead = sqlx::query(
            "UPDATE settlement_jobs
                SET status = 'dead', last_error = 'lease expired on final attempt',
                    lease_owner = NULL, lease_expires_ms = NULL, updated_at_ms = ?1
              WHERE status = 'leased' AND lease_expires_ms <= ?1 AND attempts >= max_attempts",
        )
        .bind(now_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        let requeued = sqlx::query(
            "UPDATE settlement_jobs
                SET status = 'queued', available_at_ms = ?1, last_error = 'lease expired',
                    lease_owner = NULL, lease_expires_ms = NULL, updated_at_ms = ?1
              WHERE status = 'leased' AND lease_expires_ms <= ?1",
        )
        .bind(now_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if dead + requeued > 0 {
            tracing::warn!(dead, requeued, "reclaimed expired settlement job leases");
        }
        Ok(dead + requeued)
    }

    /// Moves a dead job back to the queue with a fresh attempt budget.
    pub async fn requeue_dead(&self, id: &str, now_ms: i64) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE settlement_jobs
                SET status = 'queued', attempts = 0, available_at_ms = ?1, updated_at_ms = ?1
              WHERE id = ?2 AND status = 'dead'",
        )
        .bind(now_ms)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Per-class depth and the age of the oldest ready job.
    pub async fn stats(&self, now_ms: i64) -> Result<QueueStats, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT class,
                    SUM(CASE WHEN status = 'queued' THEN 1 ELSE 0 END) AS queued,
                    SUM(CASE WHEN status = 'leased' THEN 1 ELSE 0 END) AS leased,
                    SUM(CASE WHEN status = 'dead' THEN 1 ELSE 0 END) AS dead,
                    MIN(CASE WHEN status = 'queued' AND available_at_ms <= ?1
                             THEN available_at_ms END) AS oldest_ready
               FROM settlement_jobs
              WHERE status IN ('queued', 'leased', 'dead')
              GROUP BY class",
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await?;

        let mut stats = QueueStats::default();
        for row in rows {
            let Some(class) = PriorityClass::parse(row.get::<&str, _>("class")) else {
                continue;
            };
            let s = stats.class_mut(class);
            s.queued = row.get::<i64, _>("queued") as u64;
            s.leased = row.get::<i64, _>("leased") as u64;
            s.dead = row.get::<i64, _>("dead") as u64;
            s.oldest_ready_age_ms = row
                .get::<Option<i64>, _>("oldest_ready")
                .map(|t| now_ms - t);
        }
        Ok(stats)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0012_settlement_queue.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    fn small_config() -> QueueConfig {
        QueueConfig {
            classes: [
                ClassPolicy {
                    weight: 8,
                    capacity: 100,
                    max_leased: 100,
                },
                ClassPolicy {
                    weight: 3,
                    capacity: 100,
                    max_leased: 100,
                },
                ClassPolicy {
                    weight: 1,
                    capacity: 100,
                    max_leased: 100,
                },
            ],
            shed_bulk_at: 1_000,
            shed_normal_at: 1_000,
            ..QueueConfig::default()
        }
    }

    #[test]
    fn kinds_map_to_expected_classes() {
        assert_eq!(JobKind::Liquidation.class(), PriorityClass::Urgent);
        assert_eq!(JobKind::Expiry.class(), PriorityClass::Urgent);
        assert_eq!(JobKind::Settlement.class(), PriorityClass::Normal);
        assert_eq!(JobKind::Backfill.class(), PriorityClass::Bulk);
        assert_eq!(JobKind::OracleRepublish.class(), PriorityClass::Bulk);
        for k in [
            JobKind::Liquidation,
            JobKind::Expiry,
            JobKind::Settlement,
            JobKind::Backfill,
            JobKind::OracleRepublish,
        ] {
            assert_eq!(JobKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn scheduler_gives_each_class_its_weighted_share() {
        let mut s = FairScheduler::new(&QueueConfig::default());
        let mut counts = [0; 3];
        for _ in 0..120 {
            counts[s.pick([true; 3]).unwrap().index()] += 1;
        }
        assert_eq!(counts, [80, 30, 10]);
    }

    #[test]
    fn scheduler_never_lets_bulk_run_twice_in_a_row_while_urgent_waits() {
        let mut s = FairScheduler::new(&QueueConfig::default());
        let picks: Vec<_> = (0..48)
            .map(|_| s.pick([true, false, true]).unwrap())
            .collect();
        for w in picks.windows(2) {
            assert!(!(w[0] == PriorityClass::Bulk && w[1] == PriorityClass::Bulk));
        }
        assert!(
            picks.contains(&PriorityClass::Bulk),
            "bulk must not starve either"
        );
    }

    #[test]
    fn scheduler_serves_only_class_with_work() {
        let mut s = FairScheduler::new(&QueueConfig::default());
        for _ in 0..10 {
            assert_eq!(s.pick([false, false, true]), Some(PriorityClass::Bulk));
        }
        assert_eq!(s.pick([false; 3]), None);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let c = QueueConfig::default();
        assert_eq!(c.backoff_ms(1), 1_000);
        assert_eq!(c.backoff_ms(2), 2_000);
        assert_eq!(c.backoff_ms(4), 8_000);
        assert_eq!(c.backoff_ms(60), c.max_backoff_ms);
    }

    #[tokio::test]
    async fn urgent_work_is_served_despite_bulk_backlog() {
        let q = SettlementQueue::new(pool().await, small_config());
        for i in 0..50 {
            q.enqueue(JobKind::Backfill, &json!({ "i": i }), None, 0)
                .await
                .unwrap();
        }
        q.enqueue(JobKind::Liquidation, &json!({ "pos": "p1" }), None, 1)
            .await
            .unwrap();

        let first = q.claim("w1", 10).await.unwrap().unwrap();
        assert_eq!(first.kind, JobKind::Liquidation);
        assert_eq!(first.attempt, 1);
    }

    #[tokio::test]
    async fn bulk_lease_cap_reserves_workers_for_urgent_work() {
        let mut config = small_config();
        config.classes[PriorityClass::Bulk.index()].max_leased = 2;
        let q = SettlementQueue::new(pool().await, config);
        for i in 0..10 {
            q.enqueue(JobKind::OracleRepublish, &json!({ "i": i }), None, 0)
                .await
                .unwrap();
        }
        assert!(q.claim("w1", 1).await.unwrap().is_some());
        assert!(q.claim("w2", 1).await.unwrap().is_some());
        // Bulk is at its lease cap: a third worker stays free.
        assert!(q.claim("w3", 1).await.unwrap().is_none());

        q.enqueue(JobKind::Expiry, &json!({}), None, 2)
            .await
            .unwrap();
        assert_eq!(
            q.claim("w3", 3).await.unwrap().unwrap().kind,
            JobKind::Expiry
        );
    }

    #[tokio::test]
    async fn bulk_is_shed_before_urgent() {
        let mut config = small_config();
        config.shed_bulk_at = 3;
        config.shed_normal_at = 4;
        let q = SettlementQueue::new(pool().await, config);
        for _ in 0..3 {
            q.enqueue(JobKind::Expiry, &json!({}), None, 0)
                .await
                .unwrap();
        }
        assert!(matches!(
            q.enqueue(JobKind::Backfill, &json!({}), None, 0).await,
            Err(EnqueueError::Shed {
                class: PriorityClass::Bulk,
                total_depth: 3,
                threshold: 3
            })
        ));
        q.enqueue(JobKind::Settlement, &json!({}), None, 0)
            .await
            .unwrap();
        assert!(matches!(
            q.enqueue(JobKind::Settlement, &json!({}), None, 0).await,
            Err(EnqueueError::Shed {
                class: PriorityClass::Normal,
                ..
            })
        ));
        // Urgent work is still accepted.
        q.enqueue(JobKind::Liquidation, &json!({}), None, 0)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn class_capacity_is_a_hard_bound() {
        let mut config = small_config();
        config.classes[PriorityClass::Urgent.index()].capacity = 2;
        let q = SettlementQueue::new(pool().await, config);
        q.enqueue(JobKind::Liquidation, &json!({}), None, 0)
            .await
            .unwrap();
        q.enqueue(JobKind::Expiry, &json!({}), None, 0)
            .await
            .unwrap();
        assert!(matches!(
            q.enqueue(JobKind::Liquidation, &json!({}), None, 0).await,
            Err(EnqueueError::QueueFull {
                class: PriorityClass::Urgent,
                depth: 2,
                capacity: 2
            })
        ));
        // Finishing a job frees capacity.
        let job = q.claim("w", 1).await.unwrap().unwrap();
        assert!(q.complete(&job.id, "w", 2).await.unwrap());
        q.enqueue(JobKind::Liquidation, &json!({}), None, 3)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dedupe_key_collapses_active_duplicates_only() {
        let q = SettlementQueue::new(pool().await, small_config());
        let a = q
            .enqueue(JobKind::Expiry, &json!({}), Some("expiry:BTC:1"), 0)
            .await
            .unwrap();
        let b = q
            .enqueue(JobKind::Expiry, &json!({}), Some("expiry:BTC:1"), 0)
            .await
            .unwrap();
        assert!(matches!(a, EnqueueOutcome::Enqueued(_)));
        assert_eq!(b, EnqueueOutcome::Duplicate(a.id().to_string()));

        let job = q.claim("w", 1).await.unwrap().unwrap();
        q.complete(&job.id, "w", 2).await.unwrap();
        let c = q
            .enqueue(JobKind::Expiry, &json!({}), Some("expiry:BTC:1"), 3)
            .await
            .unwrap();
        assert!(matches!(c, EnqueueOutcome::Enqueued(_)));
    }

    #[tokio::test]
    async fn failures_back_off_then_dead_letter() {
        let mut config = small_config();
        config.max_attempts = 2;
        let q = SettlementQueue::new(pool().await, config);
        q.enqueue(JobKind::Settlement, &json!({}), None, 0)
            .await
            .unwrap();

        let job = q.claim("w", 0).await.unwrap().unwrap();
        let out = q.fail(&job.id, "w", "rpc timeout", 0).await.unwrap();
        assert_eq!(
            out,
            FailOutcome::Retrying {
                available_at_ms: 1_000
            }
        );
        assert!(
            q.claim("w", 999).await.unwrap().is_none(),
            "still backing off"
        );

        let job = q.claim("w", 1_000).await.unwrap().unwrap();
        assert_eq!(job.attempt, 2);
        assert_eq!(
            q.fail(&job.id, "w", "rpc timeout", 1_000).await.unwrap(),
            FailOutcome::Dead
        );
        assert_eq!(q.stats(1_000).await.unwrap().normal.dead, 1);

        assert!(q.requeue_dead(&job.id, 2_000).await.unwrap());
        assert_eq!(q.claim("w", 2_000).await.unwrap().unwrap().attempt, 1);
    }

    #[tokio::test]
    async fn expired_lease_is_reclaimed_and_stale_worker_cannot_complete() {
        let q = SettlementQueue::new(pool().await, small_config());
        q.enqueue(JobKind::Liquidation, &json!({}), None, 0)
            .await
            .unwrap();
        let job = q.claim("dead-worker", 0).await.unwrap().unwrap();

        let lease = q.config().lease_ms;
        let again = q.claim("w2", lease).await.unwrap().unwrap();
        assert_eq!(again.id, job.id);
        assert_eq!(again.attempt, 2);

        assert!(!q.complete(&job.id, "dead-worker", lease + 1).await.unwrap());
        assert_eq!(
            q.fail(&job.id, "dead-worker", "x", lease + 1)
                .await
                .unwrap(),
            FailOutcome::LeaseLost
        );
        assert!(q.heartbeat(&job.id, "w2", lease + 1).await.unwrap());
        assert!(q.complete(&job.id, "w2", lease + 2).await.unwrap());
    }

    #[tokio::test]
    async fn stats_report_depth_and_oldest_ready_age() {
        let q = SettlementQueue::new(pool().await, small_config());
        q.enqueue(JobKind::Backfill, &json!({}), None, 100)
            .await
            .unwrap();
        q.enqueue(JobKind::Backfill, &json!({}), None, 400)
            .await
            .unwrap();
        q.enqueue(JobKind::Expiry, &json!({}), None, 500)
            .await
            .unwrap();
        let s = q.stats(1_000).await.unwrap();
        assert_eq!(s.bulk.queued, 2);
        assert_eq!(s.bulk.oldest_ready_age_ms, Some(900));
        assert_eq!(s.urgent.queued, 1);
        assert_eq!(s.normal, ClassStats::default());
        assert_eq!(s.total_active(), 3);
    }

    #[tokio::test]
    async fn queue_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("sq-{}.db", uuid::Uuid::new_v4()));
        let url = format!("sqlite://{}?mode=rwc", dir.display());
        {
            let p = SqlitePool::connect(&url).await.unwrap();
            sqlx::raw_sql(include_str!("../migrations/0012_settlement_queue.sql"))
                .execute(&p)
                .await
                .unwrap();
            let q = SettlementQueue::new(p.clone(), small_config());
            q.enqueue(JobKind::Expiry, &json!({ "series": "BTC-1" }), None, 0)
                .await
                .unwrap();
            p.close().await;
        }
        let q = SettlementQueue::new(SqlitePool::connect(&url).await.unwrap(), small_config());
        let job = q.claim("w", 1).await.unwrap().unwrap();
        assert_eq!(job.payload, json!({ "series": "BTC-1" }));
        let _ = std::fs::remove_file(dir);
    }
}
