//! Chain cost attribution, budget guardrails and unit-economics reporting
//! (issue #230).
//!
//! Every on-chain operation (submission, fee bump, footprint restore, TTL
//! extension, storage write, WASM upload) is recorded as an append-only cost
//! event. Each event is attributed to the business action that caused it
//! and carries the true post-refund net cost.
//!
//! * [`model`]: cost taxonomy and net-cost computation.
//! * [`budget`]: per-category, per-period budget policy and states.
//! * [`ledger`]: the durable SQLite ledger and guardrail.
//! * [`report`]: reporting queries, admin routes and Prometheus metrics.
//!
//! Submitters call [`admit`] before sending a transaction and [`complete`]
//! once its result is known, including failures and preflight rejections.
//! See `docs/chain-costs.md` for the taxonomy and the budget policy.

pub mod budget;
pub mod ledger;
pub mod model;
pub mod report;

pub use budget::{BudgetPolicy, BudgetState, BudgetStatus, Period};
pub use ledger::{CostLedger, GuardrailError, Reservation};
pub use model::{
    compute_cost, Attribution, BusinessAction, CostBreakdown, CostError, Criticality, FeeReport,
    NewCostEvent, OperationType, SubmissionStatus,
};

/// A submission that has passed the guardrail and holds a reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedSubmission {
    pub operation: OperationType,
    pub attribution: Attribution,
    pub criticality: Criticality,
    pub max_fee_bid: i64,
    pub reservation: Reservation,
}

#[derive(Debug)]
pub enum CompleteError {
    Cost(CostError),
    Db(sqlx::Error),
}

impl std::fmt::Display for CompleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompleteError::Cost(e) => write!(f, "{e}"),
            CompleteError::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CompleteError {}

/// Guardrail hook for the submission path. Reserves `max_fee_bid` against
/// the action's budget. Critical traffic is always admitted.
/// `criticality = None` uses the action's default.
pub async fn admit(
    ledger: &CostLedger,
    operation: OperationType,
    attribution: Attribution,
    criticality: Option<Criticality>,
    max_fee_bid: i64,
    now_ms: i64,
) -> Result<AdmittedSubmission, GuardrailError> {
    let criticality = criticality.unwrap_or_else(|| attribution.action.default_criticality());
    let reservation = ledger
        .reserve(attribution.action, criticality, max_fee_bid, now_ms)
        .await?;
    Ok(AdmittedSubmission {
        operation,
        attribution,
        criticality,
        max_fee_bid,
        reservation,
    })
}

/// Record the result of an admitted submission. Returns the cost event id
/// and the computed breakdown.
///
/// If the fee figures are inconsistent, the attempt is still recorded,
/// conservatively at the full bid, and the error is returned so the
/// discrepancy is investigated. A cost is never silently dropped.
pub async fn complete(
    ledger: &CostLedger,
    admitted: &AdmittedSubmission,
    status: SubmissionStatus,
    tx_hash: Option<String>,
    ledger_seq: Option<u32>,
    fees: &FeeReport,
    now_ms: i64,
) -> Result<(String, CostBreakdown), CompleteError> {
    let (cost, err) = match compute_cost(status, fees) {
        Ok(c) => (c, None),
        Err(e) => {
            tracing::error!(error = %e, action_id = %admitted.attribution.action_id, "inconsistent fee report");
            let worst = CostBreakdown {
                net_cost: admitted.max_fee_bid,
                reconciled: false,
                ..Default::default()
            };
            (worst, Some(e))
        }
    };
    let event = NewCostEvent {
        operation: admitted.operation,
        status,
        tx_hash,
        ledger: ledger_seq,
        attribution: admitted.attribution.clone(),
        criticality: admitted.criticality,
        max_fee_bid: admitted.max_fee_bid,
        cost,
    };
    let id = ledger
        .record(&event, Some(&admitted.reservation), now_ms)
        .await
        .map_err(CompleteError::Db)?;
    match err {
        Some(e) => Err(CompleteError::Cost(e)),
        None => Ok((id, cost)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::str::FromStr;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// 2026-10-21T12:00:00Z (a Wednesday).
    const NOW: i64 = 20_747 * budget::DAY_MS + budget::DAY_MS / 2;

    async fn ledger_with(pool: SqlitePool) -> CostLedger {
        sqlx::raw_sql(include_str!(
            "../../migrations/0013_chain_cost_attribution.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        CostLedger::new(pool)
    }

    async fn memory_ledger() -> CostLedger {
        ledger_with(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap(),
        )
        .await
    }

    fn keeper_policy(limit: i64) -> BudgetPolicy {
        BudgetPolicy {
            category: BusinessAction::KeeperRun,
            period: Period::Daily,
            limit_stroops: limit,
            warn_ratio_bps: 8_000,
            throttle_per_minute: 2,
        }
    }

    fn success_fees(net: i64) -> FeeReport {
        FeeReport {
            max_fee_bid: net,
            inclusion_fee_charged: net,
            fee_charged: Some(net),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn records_attributed_costs_and_reports_per_settlement() {
        let l = memory_ledger().await;
        let mut attr = Attribution::new(BusinessAction::SettlementBatch, "batch-7");
        attr.series = Some("XLM-27DEC26-0.12-C".into());
        attr.job_id = Some("job-1".into());

        let a = admit(
            &l,
            OperationType::Submission,
            attr.clone(),
            None,
            300_000,
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(a.criticality, Criticality::Critical);
        let fees = FeeReport {
            max_fee_bid: 300_000,
            inclusion_fee_charged: 100,
            resource_fee_non_refundable: 40_000,
            resource_fee_refundable_max: 100_000,
            rent_fee_charged: 60_000,
            other_refundable_charged: 0,
            fee_charged: Some(100_100),
        };
        let (_, cost) = complete(
            &l,
            &a,
            SubmissionStatus::Success,
            Some("abc".into()),
            Some(9),
            &fees,
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(cost.net_cost, 100_100);

        // A failed attempt in the same batch is still attributed.
        let b = admit(
            &l,
            OperationType::Submission,
            attr.clone(),
            None,
            300_000,
            NOW,
        )
        .await
        .unwrap();
        let failed = FeeReport {
            fee_charged: Some(40_100),
            ..fees
        };
        complete(
            &l,
            &b,
            SubmissionStatus::FailedOnChain,
            Some("def".into()),
            Some(10),
            &failed,
            NOW + 1,
        )
        .await
        .unwrap();
        let t = admit(&l, OperationType::ExtendTtl, attr, None, 50_000, NOW)
            .await
            .unwrap();
        complete(
            &l,
            &t,
            SubmissionStatus::Success,
            None,
            None,
            &success_fees(20_000),
            NOW + 2,
        )
        .await
        .unwrap();

        let r = report::entity_cost(l.pool(), "settlement_batch", "batch-7")
            .await
            .unwrap();
        assert_eq!(r.tx_count, 3);
        assert_eq!(r.failed_tx_count, 1);
        assert_eq!(r.net_cost, 100_100 + 40_100 + 20_000);
        assert_eq!(
            r.by_operation
                .iter()
                .map(|o| o.operation.as_str())
                .collect::<Vec<_>>(),
            ["extend_ttl", "submission"]
        );

        let daily = report::daily_costs(l.pool(), NOW - budget::DAY_MS, NOW + budget::DAY_MS)
            .await
            .unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(
            (daily[0].day.as_str(), daily[0].tx_count),
            ("2026-10-21", 3)
        );
        let by_series = report::grouped(l.pool(), report::Dimension::Series, NOW - 1, NOW + 10)
            .await
            .unwrap();
        assert_eq!(by_series[0].key.as_deref(), Some("XLM-27DEC26-0.12-C"));

        // Reservations are fully converted to spend.
        let row: (i64, i64) = sqlx::query_as("SELECT spent, reserved FROM chain_budget_periods")
            .fetch_one(l.pool())
            .await
            .unwrap();
        assert_eq!(row, (160_200, 0));
    }

    #[tokio::test]
    async fn guardrail_trips_and_clears_with_the_period() {
        let l = memory_ledger().await;
        l.set_policy(&keeper_policy(1_000), NOW).await.unwrap();
        let attr = Attribution::new(BusinessAction::KeeperRun, "run-1");

        // 750 committed two minutes ago: below warn (800), outside the
        // throttle window.
        let earlier = NOW - 120_000;
        let a = admit(
            &l,
            OperationType::Submission,
            attr.clone(),
            None,
            750,
            earlier,
        )
        .await
        .unwrap();
        assert_eq!(a.reservation.state, BudgetState::Ok);
        complete(
            &l,
            &a,
            SubmissionStatus::Success,
            None,
            None,
            &success_fees(750),
            earlier,
        )
        .await
        .unwrap();

        // Into warn: throttled to 2 non-critical reservations per minute.
        for _ in 0..2 {
            let r = admit(&l, OperationType::Submission, attr.clone(), None, 50, NOW)
                .await
                .unwrap();
            assert_eq!(r.reservation.state, BudgetState::Warn);
        }
        let throttled = admit(&l, OperationType::Submission, attr.clone(), None, 50, NOW)
            .await
            .unwrap_err();
        assert!(matches!(throttled, GuardrailError::Throttled { .. }));

        // A minute later the throttle allows more, but the hard limit holds.
        let over = admit(
            &l,
            OperationType::Submission,
            attr.clone(),
            None,
            300,
            NOW + 61_000,
        )
        .await
        .unwrap_err();
        assert_eq!(
            over,
            GuardrailError::HardStop {
                category: BusinessAction::KeeperRun,
                committed: 850,
                limit: 1_000
            }
        );

        // Critical traffic (e.g. emergency pause on the keeper budget) still goes through.
        let crit = admit(
            &l,
            OperationType::Submission,
            attr.clone(),
            Some(Criticality::Critical),
            500,
            NOW + 62_000,
        )
        .await
        .unwrap();
        assert_eq!(crit.reservation.state, BudgetState::HardStop);

        let levels: Vec<String> =
            sqlx::query_scalar("SELECT level FROM chain_budget_breaches ORDER BY level")
                .fetch_all(l.pool())
                .await
                .unwrap();
        assert_eq!(levels, ["critical_overrun", "hard_stop", "warn"]);
        let metrics = l.render_metrics(NOW + 62_000).await.unwrap();
        assert!(metrics.contains("zenith_chain_budget_state{category=\"keeper_run\"} 2"));
        assert!(metrics.contains(
            "zenith_chain_budget_rejections_total{category=\"keeper_run\",reason=\"hard_stop\"} 1"
        ));

        // The next UTC day starts a fresh period: the guardrail clears.
        let tomorrow = NOW + budget::DAY_MS;
        let fresh = admit(&l, OperationType::Submission, attr, None, 300, tomorrow)
            .await
            .unwrap();
        assert_eq!(fresh.reservation.state, BudgetState::Ok);
    }

    #[tokio::test]
    async fn released_and_stale_reservations_free_budget() {
        let l = memory_ledger().await;
        l.set_policy(&keeper_policy(1_000), NOW).await.unwrap();
        let attr = Attribution::new(BusinessAction::KeeperRun, "run-1");
        let a = admit(&l, OperationType::Submission, attr.clone(), None, 900, NOW)
            .await
            .unwrap();
        assert!(
            admit(&l, OperationType::Submission, attr.clone(), None, 200, NOW)
                .await
                .is_err()
        );
        l.release(&a.reservation, NOW).await.unwrap();
        // Releasing twice is harmless.
        l.release(&a.reservation, NOW).await.unwrap();
        let _orphan = admit(&l, OperationType::Submission, attr.clone(), None, 900, NOW)
            .await
            .unwrap();
        assert!(
            admit(&l, OperationType::Submission, attr.clone(), None, 200, NOW)
                .await
                .is_err()
        );
        assert_eq!(l.expire_stale(60_000, NOW + 120_000).await.unwrap(), 1);
        admit(
            &l,
            OperationType::Submission,
            attr,
            None,
            200,
            NOW + 120_000,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn inconsistent_fee_report_is_recorded_at_full_bid() {
        let l = memory_ledger().await;
        let a = admit(
            &l,
            OperationType::Submission,
            Attribution::new(BusinessAction::OraclePublish, "p1"),
            None,
            5_000,
            NOW,
        )
        .await
        .unwrap();
        let bad = FeeReport {
            resource_fee_refundable_max: 10,
            rent_fee_charged: 20,
            ..Default::default()
        };
        let err = complete(&l, &a, SubmissionStatus::Success, None, None, &bad, NOW)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CompleteError::Cost(CostError::RefundableOverspent { .. })
        ));
        let net: i64 = sqlx::query_scalar("SELECT net_cost FROM chain_cost_events")
            .fetch_one(l.pool())
            .await
            .unwrap();
        assert_eq!(net, 5_000);
    }

    /// 64 workers on 8 connections race for a 1,000-stroop budget in
    /// 100-stroop reservations. Exactly 10 may win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn parallel_workers_never_overshoot_budget() {
        let path = std::env::temp_dir().join(format!("chain-cost-{}.db", uuid::Uuid::new_v4()));
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(30));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .unwrap();
        let l = Arc::new(ledger_with(pool).await);
        l.set_policy(
            &BudgetPolicy {
                throttle_per_minute: 1_000,
                ..keeper_policy(1_000)
            },
            NOW,
        )
        .await
        .unwrap();

        let mut set = tokio::task::JoinSet::new();
        for i in 0..64 {
            let l = l.clone();
            set.spawn(async move {
                let attr = Attribution::new(BusinessAction::KeeperRun, format!("run-{i}"));
                match admit(&l, OperationType::Submission, attr, None, 100, NOW).await {
                    Ok(a) => {
                        complete(
                            &l,
                            &a,
                            SubmissionStatus::Success,
                            None,
                            None,
                            &success_fees(100),
                            NOW,
                        )
                        .await
                        .unwrap();
                        true
                    }
                    Err(GuardrailError::HardStop { .. }) => false,
                    Err(e) => panic!("unexpected: {e}"),
                }
            });
        }
        let mut granted = 0;
        while let Some(r) = set.join_next().await {
            granted += r.unwrap() as i32;
        }
        assert_eq!(granted, 10);
        let (spent, reserved): (i64, i64) =
            sqlx::query_as("SELECT spent, reserved FROM chain_budget_periods")
                .fetch_one(l.pool())
                .await
                .unwrap();
        assert_eq!((spent, reserved), (1_000, 0));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn admin_routes_serve_reports() {
        let l = memory_ledger().await;
        let a = admit(
            &l,
            OperationType::Submission,
            Attribution::new(BusinessAction::SettlementBatch, "b1"),
            None,
            10,
            NOW,
        )
        .await
        .unwrap();
        complete(
            &l,
            &a,
            SubmissionStatus::Success,
            None,
            None,
            &success_fees(10),
            NOW,
        )
        .await
        .unwrap();
        let app = report::router(Arc::new(l));

        let resp = app
            .clone()
            .oneshot(Request::get("/settlements/b1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["net_cost"], 10);

        let resp = app
            .clone()
            .oneshot(
                Request::get("/settlements/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/operations?from_ms={NOW}&to_ms={}", NOW - 1))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = app
            .oneshot(Request::get("/budgets").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
