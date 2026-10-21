//! Durable cost ledger and budget guardrail, backed by SQLite.
//!
//! Submission flow:
//!
//! 1. [`CostLedger::reserve`] before submitting, with the fee bid as the
//!    estimate. The reservation is a single conditional `UPDATE` inside a
//!    write transaction, so parallel workers (and several processes sharing
//!    the database) cannot push `spent + reserved` past the limit.
//! 2. Submit.
//! 3. [`CostLedger::record`] with the result. This appends the cost event,
//!    updates the daily rollup, and turns the reservation into actual spend,
//!    all in one transaction.
//!
//! If the process dies between 1 and 3, [`CostLedger::expire_stale`]
//! releases the orphaned reservation.

use std::collections::BTreeMap;
use std::sync::Mutex;

use sqlx::{Row, SqlitePool};

use super::budget::{BudgetPolicy, BudgetState, BudgetStatus, Period, DAY_MS};
use super::model::{BusinessAction, CostBreakdown, Criticality, NewCostEvent};

const MINUTE_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardrailError {
    /// The submission would push the category past its hard limit.
    HardStop {
        category: BusinessAction,
        committed: i64,
        limit: i64,
    },
    /// The category is in `warn` and the non-critical per-minute quota is used up.
    Throttled {
        category: BusinessAction,
        per_minute: u32,
    },
    InvalidEstimate,
    Db(String),
}

impl std::fmt::Display for GuardrailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardrailError::HardStop {
                category,
                committed,
                limit,
            } => write!(
                f,
                "chain budget hard stop for {}: {committed} of {limit} stroops committed",
                category.as_str()
            ),
            GuardrailError::Throttled {
                category,
                per_minute,
            } => {
                write!(
                    f,
                    "chain budget for {} is in warn; throttled to {per_minute}/min",
                    category.as_str()
                )
            }
            GuardrailError::InvalidEstimate => write!(f, "cost estimate must be non-negative"),
            GuardrailError::Db(e) => write!(f, "budget store error: {e}"),
        }
    }
}

impl std::error::Error for GuardrailError {}

impl From<sqlx::Error> for GuardrailError {
    fn from(e: sqlx::Error) -> Self {
        GuardrailError::Db(e.to_string())
    }
}

/// A granted budget reservation. Pass it to [`CostLedger::record`] or
/// [`CostLedger::release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub id: String,
    pub category: BusinessAction,
    pub period_start_ms: i64,
    pub amount: i64,
    pub criticality: Criticality,
    /// Budget state after this reservation was granted.
    pub state: BudgetState,
}

/// In-process counters for metrics. Durable state lives in the database;
/// these exist so a rejection is visible in metrics as soon as it happens.
#[derive(Default)]
struct Counters {
    rejections: BTreeMap<(BusinessAction, &'static str), u64>,
    breaches: BTreeMap<(BusinessAction, &'static str), u64>,
}

pub struct CostLedger {
    pool: SqlitePool,
    counters: Mutex<Counters>,
}

impl CostLedger {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            counters: Mutex::new(Counters::default()),
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn set_policy(&self, policy: &BudgetPolicy, now_ms: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO chain_budget_policies \
                 (category, period, limit_stroops, warn_ratio_bps, throttle_per_minute, updated_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (category) DO UPDATE SET period = excluded.period, \
                 limit_stroops = excluded.limit_stroops, warn_ratio_bps = excluded.warn_ratio_bps, \
                 throttle_per_minute = excluded.throttle_per_minute, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(policy.category.as_str())
        .bind(policy.period.as_str())
        .bind(policy.limit_stroops)
        .bind(policy.warn_ratio_bps as i64)
        .bind(policy.throttle_per_minute as i64)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn policies(&self) -> Result<Vec<BudgetPolicy>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT category, period, limit_stroops, warn_ratio_bps, throttle_per_minute \
             FROM chain_budget_policies ORDER BY category",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().filter_map(policy_from_row).collect())
    }

    async fn policy(&self, category: BusinessAction) -> Result<Option<BudgetPolicy>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT category, period, limit_stroops, warn_ratio_bps, throttle_per_minute \
             FROM chain_budget_policies WHERE category = ?",
        )
        .bind(category.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().and_then(policy_from_row))
    }

    /// Reserve `estimate` stroops of `category`'s budget before a submission.
    ///
    /// A category with no policy is unbudgeted: the reservation is still
    /// tracked (so spend shows up), but nothing is enforced.
    pub async fn reserve(
        &self,
        category: BusinessAction,
        criticality: Criticality,
        estimate: i64,
        now_ms: i64,
    ) -> Result<Reservation, GuardrailError> {
        if estimate < 0 {
            return Err(GuardrailError::InvalidEstimate);
        }
        let policy = self.policy(category).await?;
        let period = policy.map(|p| p.period).unwrap_or(Period::Daily);
        let period_start = period.start_ms(now_ms);
        let id = uuid::Uuid::new_v4().to_string();

        let mut tx = self.pool.begin().await?;
        // The first statement is a write, so this transaction takes SQLite's
        // write lock right away and reservations are serialised.
        sqlx::query(
            "INSERT OR IGNORE INTO chain_budget_periods (category, period_start_ms) VALUES (?, ?)",
        )
        .bind(category.as_str())
        .bind(period_start)
        .execute(&mut *tx)
        .await?;

        let enforce = policy.is_some() && criticality == Criticality::NonCritical;
        let granted = if let (true, Some(p)) = (enforce, policy) {
            // Within the hard limit AND (below warn after this reservation OR
            // under the per-minute throttle).
            sqlx::query(
                "UPDATE chain_budget_periods SET reserved = reserved + ?1 \
                 WHERE category = ?2 AND period_start_ms = ?3 \
                   AND spent + reserved + ?1 <= ?4 \
                   AND (spent + reserved + ?1 < ?5 OR \
                        (SELECT COUNT(*) FROM chain_budget_reservations \
                         WHERE category = ?2 AND criticality = 'non_critical' AND created_at_ms > ?6) < ?7)",
            )
            .bind(estimate)
            .bind(category.as_str())
            .bind(period_start)
            .bind(p.limit_stroops)
            .bind(p.warn_threshold())
            .bind(now_ms - MINUTE_MS)
            .bind(p.throttle_per_minute as i64)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1
        } else {
            sqlx::query(
                "UPDATE chain_budget_periods SET reserved = reserved + ? WHERE category = ? AND period_start_ms = ?",
            )
            .bind(estimate)
            .bind(category.as_str())
            .bind(period_start)
            .execute(&mut *tx)
            .await?;
            true
        };

        let (spent, reserved) = period_totals(&mut tx, category, period_start).await?;

        if !granted {
            tx.rollback().await?;
            let p = policy.expect("enforced only with a policy");
            let committed = spent + reserved;
            let err = if committed + estimate > p.limit_stroops {
                self.bump(true, category, "hard_stop");
                self.record_breach(
                    category,
                    period_start,
                    "hard_stop",
                    committed + estimate,
                    p.limit_stroops,
                    now_ms,
                )
                .await;
                tracing::error!(
                    category = category.as_str(),
                    committed,
                    estimate,
                    limit = p.limit_stroops,
                    "chain budget hard stop: non-critical submission rejected"
                );
                GuardrailError::HardStop {
                    category,
                    committed,
                    limit: p.limit_stroops,
                }
            } else {
                self.bump(true, category, "throttled");
                tracing::warn!(
                    category = category.as_str(),
                    "chain budget throttling non-critical submission"
                );
                GuardrailError::Throttled {
                    category,
                    per_minute: p.throttle_per_minute,
                }
            };
            return Err(err);
        }

        sqlx::query(
            "INSERT INTO chain_budget_reservations \
                 (id, category, period_start_ms, amount, criticality, created_at_ms) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(category.as_str())
        .bind(period_start)
        .bind(estimate)
        .bind(criticality.as_str())
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let state = match policy {
            Some(p) => {
                let state = p.state(spent + reserved);
                self.observe_state(&p, criticality, period_start, spent + reserved, now_ms)
                    .await;
                state
            }
            None => BudgetState::Ok,
        };
        Ok(Reservation {
            id,
            category,
            period_start_ms: period_start,
            amount: estimate,
            criticality,
            state,
        })
    }

    /// Release a reservation whose submission never happened.
    pub async fn release(&self, reservation: &Reservation, now_ms: i64) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE chain_budget_reservations SET settled_at_ms = ?, actual_cost = 0 \
             WHERE id = ? AND settled_at_ms IS NULL",
        )
        .bind(now_ms)
        .bind(&reservation.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            sqlx::query(
                "UPDATE chain_budget_periods SET reserved = reserved - ? WHERE category = ? AND period_start_ms = ?",
            )
            .bind(reservation.amount)
            .bind(reservation.category.as_str())
            .bind(reservation.period_start_ms)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// Append a cost event, update the rollup and settle the reservation.
    /// Returns the event id. Recording the same reservation twice only
    /// counts its spend once.
    pub async fn record(
        &self,
        event: &NewCostEvent,
        reservation: Option<&Reservation>,
        now_ms: i64,
    ) -> Result<String, sqlx::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let a = &event.attribution;
        let c: &CostBreakdown = &event.cost;
        // Looked up before the transaction so it never waits on a pooled
        // connection while holding the write lock.
        let unreserved_period = match reservation {
            Some(_) => None,
            None => Some(
                self.policy(a.action)
                    .await?
                    .map(|p| p.period)
                    .unwrap_or(Period::Daily),
            ),
        };
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO chain_cost_events \
                 (id, recorded_at_ms, operation, status, tx_hash, ledger, action, action_id, request_id, job_id, \
                  account, series, criticality, max_fee_bid, inclusion_fee, resource_fee, rent_fee, refund, \
                  net_cost, reconciled, reservation_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(now_ms)
        .bind(event.operation.as_str())
        .bind(event.status.as_str())
        .bind(&event.tx_hash)
        .bind(event.ledger.map(|l| l as i64))
        .bind(a.action.as_str())
        .bind(&a.action_id)
        .bind(&a.request_id)
        .bind(&a.job_id)
        .bind(&a.account)
        .bind(&a.series)
        .bind(event.criticality.as_str())
        .bind(event.max_fee_bid)
        .bind(c.inclusion_fee)
        .bind(c.resource_fee)
        .bind(c.rent_fee)
        .bind(c.refund)
        .bind(c.net_cost)
        .bind(c.reconciled as i64)
        .bind(reservation.map(|r| r.id.clone()))
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO chain_cost_daily_rollup \
                 (day_index, action, operation, tx_count, inclusion_fee, resource_fee, rent_fee, refund, net_cost) \
             VALUES (?, ?, ?, 1, ?, ?, ?, ?, ?) \
             ON CONFLICT (day_index, action, operation) DO UPDATE SET \
                 tx_count = tx_count + 1, inclusion_fee = inclusion_fee + excluded.inclusion_fee, \
                 resource_fee = resource_fee + excluded.resource_fee, rent_fee = rent_fee + excluded.rent_fee, \
                 refund = refund + excluded.refund, net_cost = net_cost + excluded.net_cost",
        )
        .bind(now_ms.div_euclid(DAY_MS))
        .bind(a.action.as_str())
        .bind(event.operation.as_str())
        .bind(c.inclusion_fee)
        .bind(c.resource_fee)
        .bind(c.rent_fee)
        .bind(c.refund)
        .bind(c.net_cost)
        .execute(&mut *tx)
        .await?;

        let (category, period_start, released) = match reservation {
            Some(r) => {
                let open = sqlx::query(
                    "UPDATE chain_budget_reservations SET settled_at_ms = ?, actual_cost = ? \
                     WHERE id = ? AND settled_at_ms IS NULL",
                )
                .bind(now_ms)
                .bind(c.net_cost)
                .bind(&r.id)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 1;
                (r.category, r.period_start_ms, open.then_some(r.amount))
            }
            None => {
                let period = unreserved_period.unwrap_or(Period::Daily);
                (a.action, period.start_ms(now_ms), Some(0))
            }
        };
        if let Some(released) = released {
            sqlx::query("INSERT OR IGNORE INTO chain_budget_periods (category, period_start_ms) VALUES (?, ?)")
                .bind(category.as_str())
                .bind(period_start)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE chain_budget_periods SET spent = spent + ?, reserved = reserved - ? \
                 WHERE category = ? AND period_start_ms = ?",
            )
            .bind(c.net_cost)
            .bind(released)
            .bind(category.as_str())
            .bind(period_start)
            .execute(&mut *tx)
            .await?;
        }
        let (spent, reserved) = period_totals(&mut tx, category, period_start).await?;
        tx.commit().await?;

        if let Some(p) = self.policy(category).await? {
            self.observe_state(
                &p,
                event.criticality,
                period_start,
                spent + reserved,
                now_ms,
            )
            .await;
        }
        Ok(id)
    }

    /// Release reservations older than `max_age_ms` that were never settled
    /// (the worker died mid-submission). Returns how many were released.
    pub async fn expire_stale(&self, max_age_ms: i64, now_ms: i64) -> Result<u64, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, category, period_start_ms, amount, criticality FROM chain_budget_reservations \
             WHERE settled_at_ms IS NULL AND created_at_ms < ?",
        )
        .bind(now_ms - max_age_ms)
        .fetch_all(&self.pool)
        .await?;
        let mut n = 0;
        for row in rows {
            let Some(category) = BusinessAction::parse(row.get::<String, _>(1).as_str()) else {
                continue;
            };
            let r = Reservation {
                id: row.get(0),
                category,
                period_start_ms: row.get(2),
                amount: row.get(3),
                criticality: Criticality::parse(row.get::<String, _>(4).as_str())
                    .unwrap_or(Criticality::NonCritical),
                state: BudgetState::Ok,
            };
            self.release(&r, now_ms).await?;
            n += 1;
        }
        Ok(n)
    }

    /// Current-period status for every budgeted category.
    pub async fn statuses(&self, now_ms: i64) -> Result<Vec<BudgetStatus>, sqlx::Error> {
        let mut out = Vec::new();
        for p in self.policies().await? {
            let start = p.period.start_ms(now_ms);
            let row = sqlx::query(
                "SELECT spent, reserved FROM chain_budget_periods WHERE category = ? AND period_start_ms = ?",
            )
            .bind(p.category.as_str())
            .bind(start)
            .fetch_optional(&self.pool)
            .await?;
            let (spent, reserved) = row.map(|r| (r.get(0), r.get(1))).unwrap_or((0, 0));
            out.push(BudgetStatus::new(&p, start, spent, reserved));
        }
        Ok(out)
    }

    async fn observe_state(
        &self,
        policy: &BudgetPolicy,
        criticality: Criticality,
        period_start: i64,
        committed: i64,
        now_ms: i64,
    ) {
        let level = match policy.state(committed) {
            BudgetState::Ok => return,
            BudgetState::Warn => "warn",
            BudgetState::HardStop
                if criticality == Criticality::Critical && committed > policy.limit_stroops =>
            {
                "critical_overrun"
            }
            BudgetState::HardStop => "hard_stop",
        };
        if self
            .record_breach(
                policy.category,
                period_start,
                level,
                committed,
                policy.limit_stroops,
                now_ms,
            )
            .await
        {
            tracing::error!(
                category = policy.category.as_str(),
                level,
                committed,
                limit = policy.limit_stroops,
                "chain budget threshold crossed"
            );
        }
    }

    /// Insert the breach row once per (category, period, level). Returns
    /// whether this call was the first to trip it.
    async fn record_breach(
        &self,
        category: BusinessAction,
        period_start: i64,
        level: &'static str,
        committed: i64,
        limit: i64,
        now_ms: i64,
    ) -> bool {
        let first = sqlx::query(
            "INSERT OR IGNORE INTO chain_budget_breaches \
                 (category, period_start_ms, level, committed, limit_stroops, at_ms) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(category.as_str())
        .bind(period_start)
        .bind(level)
        .bind(committed)
        .bind(limit)
        .bind(now_ms)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected() == 1)
        .unwrap_or(false);
        if first {
            self.bump(false, category, level);
        }
        first
    }

    fn bump(&self, rejection: bool, category: BusinessAction, label: &'static str) {
        let mut c = self.counters.lock().expect("counters lock");
        let map = if rejection {
            &mut c.rejections
        } else {
            &mut c.breaches
        };
        *map.entry((category, label)).or_default() += 1;
    }

    /// Prometheus text exposition for budget state and spend.
    pub async fn render_metrics(&self, now_ms: i64) -> Result<String, sqlx::Error> {
        use std::fmt::Write;
        let mut out = String::new();
        out.push_str("# HELP zenith_chain_budget_utilization_ratio Committed spend over budget limit, current period.\n");
        out.push_str("# TYPE zenith_chain_budget_utilization_ratio gauge\n");
        let statuses = self.statuses(now_ms).await?;
        for s in &statuses {
            let ratio = if s.limit_stroops == 0 {
                0.0
            } else {
                (s.spent + s.reserved) as f64 / s.limit_stroops as f64
            };
            let _ = writeln!(
                out,
                "zenith_chain_budget_utilization_ratio{{category=\"{}\"}} {ratio}",
                s.category.as_str()
            );
        }
        out.push_str("# HELP zenith_chain_budget_state 0 = ok, 1 = warn, 2 = hard stop.\n");
        out.push_str("# TYPE zenith_chain_budget_state gauge\n");
        for s in &statuses {
            let _ = writeln!(
                out,
                "zenith_chain_budget_state{{category=\"{}\"}} {}",
                s.category.as_str(),
                s.state.gauge()
            );
        }
        out.push_str(
            "# HELP zenith_chain_cost_stroops_total Net on-chain cost by operation and action.\n",
        );
        out.push_str("# TYPE zenith_chain_cost_stroops_total counter\n");
        let rows = sqlx::query(
            "SELECT action, operation, SUM(net_cost) FROM chain_cost_daily_rollup GROUP BY action, operation \
             ORDER BY action, operation",
        )
        .fetch_all(&self.pool)
        .await?;
        for r in rows {
            let _ = writeln!(
                out,
                "zenith_chain_cost_stroops_total{{action=\"{}\",operation=\"{}\"}} {}",
                r.get::<String, _>(0),
                r.get::<String, _>(1),
                r.get::<i64, _>(2)
            );
        }
        let c = self.counters.lock().expect("counters lock");
        out.push_str("# TYPE zenith_chain_budget_rejections_total counter\n");
        for ((cat, reason), n) in &c.rejections {
            let _ = writeln!(
                out,
                "zenith_chain_budget_rejections_total{{category=\"{}\",reason=\"{reason}\"}} {n}",
                cat.as_str()
            );
        }
        out.push_str("# TYPE zenith_chain_budget_breaches_total counter\n");
        for ((cat, level), n) in &c.breaches {
            let _ = writeln!(
                out,
                "zenith_chain_budget_breaches_total{{category=\"{}\",level=\"{level}\"}} {n}",
                cat.as_str()
            );
        }
        Ok(out)
    }
}

fn policy_from_row(r: &sqlx::sqlite::SqliteRow) -> Option<BudgetPolicy> {
    Some(BudgetPolicy {
        category: BusinessAction::parse(r.get::<String, _>(0).as_str())?,
        period: Period::parse(r.get::<String, _>(1).as_str())?,
        limit_stroops: r.get(2),
        warn_ratio_bps: r.get::<i64, _>(3) as u32,
        throttle_per_minute: r.get::<i64, _>(4) as u32,
    })
}

async fn period_totals(
    conn: &mut sqlx::SqliteConnection,
    category: BusinessAction,
    period_start: i64,
) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query("SELECT spent, reserved FROM chain_budget_periods WHERE category = ? AND period_start_ms = ?")
        .bind(category.as_str())
        .bind(period_start)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.map(|r| (r.get(0), r.get(1))).unwrap_or((0, 0)))
}
