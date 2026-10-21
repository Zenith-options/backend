//! Shadow-mode settlement: run the on-chain path next to the off-chain
//! engine and record whether they agree.
//!
//! Cutover from paper trading to live settlement is the riskiest step in
//! the programme. In [`SettlementMode::Shadow`] the off-chain engine stays
//! authoritative; every margin, payoff and expiry decision it makes is also
//! evaluated on the on-chain path **by simulation only** — nothing is
//! signed or submitted — and both results are stored in
//! `shadow_comparisons`. [`cutover_readiness`] turns that evidence into a
//! go/no-go.
//!
//! After cutover, [`SettlementMode::Live`] keeps comparing, with the
//! off-chain engine as the reference, so a contract upgrade that changes
//! behaviour surfaces as divergences tagged with its contract version.
//!
//! The shadow path must never affect the authoritative one: [`ShadowRunner::observe`]
//! has a timeout, swallows nothing silently (errors are recorded as
//! `on_chain_error` verdicts) and its result is for logging only.

use async_trait::async_trait;
use serde::Serialize;
use sqlx::SqlitePool;
use std::time::Duration;

/// Which path is authoritative, and whether the other one is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMode {
    /// Off-chain only. No on-chain calls at all.
    Paper,
    /// Off-chain authoritative; on-chain simulated and compared.
    Shadow,
    /// On-chain authoritative; off-chain still computed and compared.
    Live,
}

impl SettlementMode {
    /// Parses `paper` / `shadow` / `live` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "paper" => Some(Self::Paper),
            "shadow" => Some(Self::Shadow),
            "live" => Some(Self::Live),
            _ => None,
        }
    }

    /// Reads `SETTLEMENT_MODE`; unset means `paper`. An unrecognised value
    /// is an error rather than a silent default — a typo must not flip a
    /// deployment into or out of live settlement.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("SETTLEMENT_MODE") {
            Err(_) => Ok(Self::Paper),
            Ok(v) => Self::parse(&v)
                .ok_or_else(|| format!("SETTLEMENT_MODE must be paper, shadow or live, got {v:?}")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Shadow => "shadow",
            Self::Live => "live",
        }
    }
}

/// The kinds of decision compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// Collateral / margin required for a position.
    Margin,
    /// Payoff of a closed or exercised position.
    Payoff,
    /// Expiry settlement: exercised or not, and the payout.
    Expiry,
}

impl DecisionKind {
    pub const ALL: [DecisionKind; 3] = [Self::Margin, Self::Payoff, Self::Expiry];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Margin => "margin",
            Self::Payoff => "payoff",
            Self::Expiry => "expiry",
        }
    }
}

/// One decision's result, from either path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// Amount in stroops (margin required, payoff, or expiry payout).
    pub amount: i64,
    /// Expiry only: whether the option finished in the money.
    pub exercised: Option<bool>,
}

/// A decision the off-chain engine made, with the inputs it used.
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub kind: DecisionKind,
    /// Position / series id the decision is about.
    pub subject_id: String,
    /// Everything the on-chain path needs to reproduce it (spot, strike,
    /// contracts, fixing price, ...).
    pub inputs: serde_json::Value,
    pub off_chain: Outcome,
}

/// The on-chain settlement path, evaluated without submitting anything —
/// typically a Soroban `simulateTransaction` of the contract call.
#[async_trait]
pub trait OnChainPath: Send + Sync {
    /// Identifies the deployed contract code (e.g. wasm hash).
    fn contract_version(&self) -> String;

    /// Evaluates the decision on-chain. Must not submit a transaction.
    async fn simulate(
        &self,
        kind: DecisionKind,
        subject_id: &str,
        inputs: &serde_json::Value,
    ) -> Result<Outcome, String>;
}

/// How far the two paths may differ before it counts as a divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tolerance {
    /// Absolute allowance in stroops (covers f64 → integer rounding).
    pub abs_stroops: i64,
    /// Relative allowance in basis points of the off-chain amount.
    pub rel_bps: i64,
}

impl Tolerance {
    pub const EXACT: Tolerance = Tolerance {
        abs_stroops: 0,
        rel_bps: 0,
    };

    /// The larger of the absolute and relative allowance.
    pub fn allowed(&self, reference: i64) -> i64 {
        let rel = (reference.unsigned_abs() as i128 * self.rel_bps.max(0) as i128) / 10_000;
        self.abs_stroops
            .max(0)
            .max(rel.min(i64::MAX as i128) as i64)
    }
}

/// Per-kind tolerances. The default allows 10 stroops (1e-6 units) on
/// amounts for float-vs-integer rounding and no relative slack at all; the
/// exercise decision at expiry must always match exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tolerances {
    pub margin: Tolerance,
    pub payoff: Tolerance,
    pub expiry: Tolerance,
}

impl Default for Tolerances {
    fn default() -> Self {
        let t = Tolerance {
            abs_stroops: 10,
            rel_bps: 0,
        };
        Self {
            margin: t,
            payoff: t,
            expiry: t,
        }
    }
}

impl Tolerances {
    pub fn for_kind(&self, kind: DecisionKind) -> Tolerance {
        match kind {
            DecisionKind::Margin => self.margin,
            DecisionKind::Payoff => self.payoff,
            DecisionKind::Expiry => self.expiry,
        }
    }
}

/// Result of comparing the two paths for one decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Match,
    WithinTolerance { diff: i64 },
    Diverged { diff: i64, reason: String },
    OnChainError { error: String },
}

impl Verdict {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::WithinTolerance { .. } => "within_tolerance",
            Self::Diverged { .. } => "diverged",
            Self::OnChainError { .. } => "on_chain_error",
        }
    }

    /// True for anything that should block cutover.
    pub fn is_divergent(&self) -> bool {
        matches!(self, Self::Diverged { .. } | Self::OnChainError { .. })
    }
}

/// Compares an off-chain and on-chain outcome. Pure.
pub fn compare(off: Outcome, on: Outcome, tolerance: Tolerance) -> Verdict {
    let diff = on.amount.saturating_sub(off.amount);
    if off.exercised != on.exercised {
        return Verdict::Diverged {
            diff,
            reason: format!(
                "exercise decision differs: off-chain {:?}, on-chain {:?}",
                off.exercised, on.exercised
            ),
        };
    }
    if diff == 0 {
        return Verdict::Match;
    }
    let allowed = tolerance.allowed(off.amount);
    if diff.unsigned_abs() <= allowed as u64 {
        Verdict::WithinTolerance { diff }
    } else {
        Verdict::Diverged {
            diff,
            reason: format!("amount differs by {diff} stroops (allowed {allowed})"),
        }
    }
}

/// Runs the comparison for each decision and records it.
pub struct ShadowRunner<P> {
    mode: SettlementMode,
    on_chain: P,
    pool: SqlitePool,
    tolerances: Tolerances,
    timeout: Duration,
}

impl<P: OnChainPath> ShadowRunner<P> {
    pub fn new(mode: SettlementMode, on_chain: P, pool: SqlitePool) -> Self {
        Self {
            mode,
            on_chain,
            pool,
            tolerances: Tolerances::default(),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_tolerances(mut self, tolerances: Tolerances) -> Self {
        self.tolerances = tolerances;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn mode(&self) -> SettlementMode {
        self.mode
    }

    /// Simulates `decision` on-chain, compares and records it. Returns
    /// `None` in paper mode. Never fails the caller: a simulation error or
    /// timeout is itself the recorded verdict, and a failure to persist is
    /// logged.
    pub async fn observe(&self, decision: &Decision) -> Option<Verdict> {
        if self.mode == SettlementMode::Paper {
            return None;
        }
        let simulated = tokio::time::timeout(
            self.timeout,
            self.on_chain
                .simulate(decision.kind, &decision.subject_id, &decision.inputs),
        )
        .await
        .unwrap_or_else(|_| Err(format!("simulation timed out after {:?}", self.timeout)));

        let (on, verdict) = match simulated {
            Ok(on) => (
                Some(on),
                compare(
                    decision.off_chain,
                    on,
                    self.tolerances.for_kind(decision.kind),
                ),
            ),
            Err(error) => (None, Verdict::OnChainError { error }),
        };

        if verdict.is_divergent() {
            tracing::warn!(
                mode = self.mode.as_str(),
                kind = decision.kind.as_str(),
                subject_id = %decision.subject_id,
                verdict = ?verdict,
                "shadow settlement divergence"
            );
        }
        if let Err(e) = self.record(decision, on, &verdict).await {
            tracing::error!(error = %e, "failed to record shadow comparison");
        }
        Some(verdict)
    }

    async fn record(
        &self,
        d: &Decision,
        on: Option<Outcome>,
        verdict: &Verdict,
    ) -> Result<(), sqlx::Error> {
        let (diff, detail) = match verdict {
            Verdict::Match => (Some(0), None),
            Verdict::WithinTolerance { diff } => (Some(*diff), None),
            Verdict::Diverged { diff, reason } => (Some(*diff), Some(reason.clone())),
            Verdict::OnChainError { error } => (None, Some(error.clone())),
        };
        sqlx::query(
            "INSERT INTO shadow_comparisons
               (mode, kind, subject_id, contract_version, off_chain_amount, off_chain_exercised,
                on_chain_amount, on_chain_exercised, diff, verdict, detail, inputs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(self.mode.as_str())
        .bind(d.kind.as_str())
        .bind(&d.subject_id)
        .bind(self.on_chain.contract_version())
        .bind(d.off_chain.amount)
        .bind(d.off_chain.exercised)
        .bind(on.map(|o| o.amount))
        .bind(on.and_then(|o| o.exercised))
        .bind(diff)
        .bind(verdict.as_str())
        .bind(detail)
        .bind(d.inputs.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// What has to be true before switching to live settlement.
#[derive(Debug, Clone)]
pub struct CutoverCriteria {
    /// Only evidence against this contract version counts.
    pub contract_version: String,
    /// Look-back window in hours.
    pub window_hours: u32,
    /// Minimum comparisons per decision kind.
    pub min_samples: i64,
    /// Highest tolerated divergence rate in basis points (0 = none).
    pub max_divergence_bps: i64,
}

impl CutoverCriteria {
    /// A conservative default: a week of evidence, 500 samples of each
    /// decision kind, zero divergences.
    pub fn strict(contract_version: impl Into<String>) -> Self {
        Self {
            contract_version: contract_version.into(),
            window_hours: 24 * 7,
            min_samples: 500,
            max_divergence_bps: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KindStats {
    pub kind: DecisionKind,
    pub samples: i64,
    pub matched: i64,
    pub within_tolerance: i64,
    pub diverged: i64,
    pub on_chain_errors: i64,
    /// `(diverged + on_chain_errors) / samples`, basis points.
    pub divergence_bps: Option<i64>,
    pub max_abs_diff: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CutoverReport {
    pub ready: bool,
    pub contract_version: String,
    pub window_hours: u32,
    pub kinds: Vec<KindStats>,
    /// Why `ready` is false. Empty when ready.
    pub blockers: Vec<String>,
}

/// Decides cutover readiness from the stats per kind. Pure.
pub fn evaluate_cutover(criteria: &CutoverCriteria, kinds: Vec<KindStats>) -> CutoverReport {
    let mut blockers = Vec::new();
    for k in &kinds {
        let name = k.kind.as_str();
        if k.samples < criteria.min_samples {
            blockers.push(format!(
                "{name}: {} samples, need {}",
                k.samples, criteria.min_samples
            ));
        }
        if let Some(bps) = k.divergence_bps {
            if bps > criteria.max_divergence_bps {
                blockers.push(format!(
                    "{name}: divergence rate {bps} bps exceeds {} bps ({} diverged, {} errors)",
                    criteria.max_divergence_bps, k.diverged, k.on_chain_errors
                ));
            }
        }
    }
    CutoverReport {
        ready: blockers.is_empty(),
        contract_version: criteria.contract_version.clone(),
        window_hours: criteria.window_hours,
        kinds,
        blockers,
    }
}

/// Builds the cutover report from recorded comparisons.
pub async fn cutover_readiness(
    pool: &SqlitePool,
    criteria: &CutoverCriteria,
) -> Result<CutoverReport, sqlx::Error> {
    let window = format!("-{} hours", criteria.window_hours);
    let mut kinds = Vec::new();
    for kind in DecisionKind::ALL {
        let (samples, matched, within, diverged, errors, max_abs): (i64, i64, i64, i64, i64, i64) =
            sqlx::query_as(
                "SELECT COUNT(*),
                        COALESCE(SUM(verdict = 'match'), 0),
                        COALESCE(SUM(verdict = 'within_tolerance'), 0),
                        COALESCE(SUM(verdict = 'diverged'), 0),
                        COALESCE(SUM(verdict = 'on_chain_error'), 0),
                        COALESCE(MAX(ABS(diff)), 0)
                   FROM shadow_comparisons
                  WHERE contract_version = ?1 AND kind = ?2
                    AND observed_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?3)",
            )
            .bind(&criteria.contract_version)
            .bind(kind.as_str())
            .bind(&window)
            .fetch_one(pool)
            .await?;
        kinds.push(KindStats {
            kind,
            samples,
            matched,
            within_tolerance: within,
            diverged,
            on_chain_errors: errors,
            divergence_bps: (samples > 0).then(|| (diverged + errors) * 10_000 / samples),
            max_abs_diff: max_abs,
        });
    }
    Ok(evaluate_cutover(criteria, kinds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Mutex;

    fn amt(amount: i64) -> Outcome {
        Outcome {
            amount,
            exercised: None,
        }
    }

    fn expiry(amount: i64, exercised: bool) -> Outcome {
        Outcome {
            amount,
            exercised: Some(exercised),
        }
    }

    #[test]
    fn mode_parsing_is_strict() {
        assert_eq!(
            SettlementMode::parse(" Shadow "),
            Some(SettlementMode::Shadow)
        );
        assert_eq!(SettlementMode::parse("LIVE"), Some(SettlementMode::Live));
        assert_eq!(SettlementMode::parse("shaddow"), None);
    }

    #[test]
    fn compare_classifies_amounts() {
        let tol = Tolerance {
            abs_stroops: 10,
            rel_bps: 0,
        };
        assert_eq!(compare(amt(1_000), amt(1_000), tol), Verdict::Match);
        assert_eq!(
            compare(amt(1_000), amt(1_010), tol),
            Verdict::WithinTolerance { diff: 10 }
        );
        assert_eq!(
            compare(amt(1_000), amt(990), tol),
            Verdict::WithinTolerance { diff: -10 }
        );
        assert!(matches!(
            compare(amt(1_000), amt(1_011), tol),
            Verdict::Diverged { diff: 11, .. }
        ));
    }

    #[test]
    fn relative_tolerance_scales_with_amount() {
        let tol = Tolerance {
            abs_stroops: 0,
            rel_bps: 10, // 0.1%
        };
        assert_eq!(tol.allowed(1_000_000), 1_000);
        assert_eq!(tol.allowed(-1_000_000), 1_000);
        assert!(matches!(
            compare(amt(1_000_000), amt(1_001_000), tol),
            Verdict::WithinTolerance { .. }
        ));
        assert!(compare(amt(1_000_000), amt(1_001_001), tol).is_divergent());
        assert_eq!(Tolerance::EXACT.allowed(i64::MIN), 0);
    }

    #[test]
    fn exercise_mismatch_diverges_even_with_equal_amounts() {
        let v = compare(expiry(0, false), expiry(0, true), Tolerance::EXACT);
        assert!(matches!(v, Verdict::Diverged { diff: 0, .. }));
    }

    fn stats(kind: DecisionKind, samples: i64, bad: i64) -> KindStats {
        KindStats {
            kind,
            samples,
            matched: samples - bad,
            within_tolerance: 0,
            diverged: bad,
            on_chain_errors: 0,
            divergence_bps: (samples > 0).then(|| bad * 10_000 / samples),
            max_abs_diff: 0,
        }
    }

    #[test]
    fn cutover_needs_samples_for_every_kind_and_no_divergence() {
        let c = CutoverCriteria {
            contract_version: "v1".into(),
            window_hours: 1,
            min_samples: 10,
            max_divergence_bps: 0,
        };
        let ok = evaluate_cutover(
            &c,
            vec![
                stats(DecisionKind::Margin, 10, 0),
                stats(DecisionKind::Payoff, 10, 0),
                stats(DecisionKind::Expiry, 10, 0),
            ],
        );
        assert!(ok.ready, "{:?}", ok.blockers);

        let not_ok = evaluate_cutover(
            &c,
            vec![
                stats(DecisionKind::Margin, 10, 1),
                stats(DecisionKind::Payoff, 10, 0),
                stats(DecisionKind::Expiry, 3, 0),
            ],
        );
        assert!(!not_ok.ready);
        assert_eq!(not_ok.blockers.len(), 2);
    }

    /// On-chain path that returns scripted results and counts calls.
    struct Scripted {
        version: &'static str,
        results: Mutex<Vec<Result<Outcome, String>>>,
        delay: Duration,
    }

    #[async_trait]
    impl OnChainPath for Scripted {
        fn contract_version(&self) -> String {
            self.version.into()
        }
        async fn simulate(
            &self,
            _: DecisionKind,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<Outcome, String> {
            tokio::time::sleep(self.delay).await;
            self.results.lock().unwrap().remove(0)
        }
    }

    fn scripted(results: Vec<Result<Outcome, String>>) -> Scripted {
        Scripted {
            version: "wasm-abc",
            results: Mutex::new(results),
            delay: Duration::ZERO,
        }
    }

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0013_shadow_mode.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    fn decision(kind: DecisionKind, off: Outcome) -> Decision {
        Decision {
            kind,
            subject_id: "pos-1".into(),
            inputs: serde_json::json!({"spot": 0.12, "strike": 0.11}),
            off_chain: off,
        }
    }

    #[tokio::test]
    async fn paper_mode_never_calls_the_chain() {
        let runner = ShadowRunner::new(SettlementMode::Paper, scripted(vec![]), fresh_pool().await);
        // An empty script would panic if simulate were called.
        assert_eq!(
            runner
                .observe(&decision(DecisionKind::Margin, amt(1)))
                .await,
            None
        );
    }

    #[tokio::test]
    async fn shadow_mode_records_every_comparison() {
        let pool = fresh_pool().await;
        let runner = ShadowRunner::new(
            SettlementMode::Shadow,
            scripted(vec![
                Ok(amt(100)),
                Ok(amt(105)),
                Ok(expiry(0, true)),
                Err("HostError: contract trapped".into()),
            ]),
            pool.clone(),
        );
        let verdicts = [
            runner
                .observe(&decision(DecisionKind::Margin, amt(100)))
                .await,
            runner
                .observe(&decision(DecisionKind::Payoff, amt(100)))
                .await,
            runner
                .observe(&decision(DecisionKind::Expiry, expiry(0, false)))
                .await,
            runner
                .observe(&decision(DecisionKind::Margin, amt(1)))
                .await,
        ];
        assert_eq!(verdicts[0], Some(Verdict::Match));
        assert_eq!(verdicts[1], Some(Verdict::WithinTolerance { diff: 5 }));
        assert!(verdicts[2].as_ref().unwrap().is_divergent());
        assert!(matches!(verdicts[3], Some(Verdict::OnChainError { .. })));

        let rows: Vec<(String, String, String, Option<i64>)> = sqlx::query_as(
            "SELECT mode, verdict, contract_version, on_chain_amount FROM shadow_comparisons ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.0 == "shadow" && r.2 == "wasm-abc"));
        assert_eq!(rows[3].1, "on_chain_error");
        assert_eq!(rows[3].3, None);
    }

    #[tokio::test]
    async fn slow_simulation_is_recorded_as_an_error() {
        let pool = fresh_pool().await;
        let runner = ShadowRunner::new(
            SettlementMode::Live,
            Scripted {
                version: "v",
                results: Mutex::new(vec![Ok(amt(1))]),
                delay: Duration::from_millis(200),
            },
            pool,
        )
        .with_timeout(Duration::from_millis(20));
        let v = runner
            .observe(&decision(DecisionKind::Payoff, amt(1)))
            .await
            .unwrap();
        assert!(matches!(v, Verdict::OnChainError { ref error } if error.contains("timed out")));
    }

    #[tokio::test]
    async fn readiness_only_counts_the_current_contract_version() {
        let pool = fresh_pool().await;
        for (version, verdict) in [
            ("old", "diverged"),
            ("new", "match"),
            ("new", "match"),
            ("new", "within_tolerance"),
        ] {
            for kind in ["margin", "payoff", "expiry"] {
                sqlx::query(
                    "INSERT INTO shadow_comparisons
                       (mode, kind, subject_id, contract_version, off_chain_amount, diff, verdict)
                     VALUES ('shadow', ?1, 's', ?2, 0, 0, ?3)",
                )
                .bind(kind)
                .bind(version)
                .bind(verdict)
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        let criteria = CutoverCriteria {
            contract_version: "new".into(),
            window_hours: 1,
            min_samples: 3,
            max_divergence_bps: 0,
        };
        let report = cutover_readiness(&pool, &criteria).await.unwrap();
        assert!(report.ready, "{:?}", report.blockers);
        assert!(report.kinds.iter().all(|k| k.samples == 3));

        let old = cutover_readiness(
            &pool,
            &CutoverCriteria {
                contract_version: "old".into(),
                min_samples: 1,
                ..criteria
            },
        )
        .await
        .unwrap();
        assert!(!old.ready);
        assert_eq!(old.kinds[0].divergence_bps, Some(10_000));
    }
}
