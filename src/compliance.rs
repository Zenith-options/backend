//! Sanctions / AML screening for value-moving flows.
//!
//! The paper-trading backend trusts every authenticated wallet. That stops
//! being acceptable once real value moves through Soroban escrow, so every
//! flow that moves funds — deposit, withdrawal, opening a position and
//! receiving a settlement payout — must pass through [`ComplianceGate`]
//! first.
//!
//! The gate is deliberately small and provider-agnostic:
//!
//! * A [`ScreeningProvider`] answers "is this address sanctioned / risky?".
//!   [`DenylistProvider`] screens against a local list (e.g. an exported
//!   OFAC SDN digital-currency address list), [`HttpScreeningProvider`]
//!   calls a vendor API, and [`CompositeProvider`] combines several with
//!   the worst verdict winning.
//! * Results are cached in `compliance_screenings` for a configurable TTL
//!   so a busy wallet is not re-screened on every request.
//! * Operators can issue explicit allow/deny overrides (with a reason and
//!   optional expiry) for false positives or manual freezes.
//! * Every decision is written to the append-only `compliance_decisions`
//!   table, so "why was this wallet allowed to withdraw?" always has an
//!   answer.
//!
//! In [`ComplianceMode::Enforce`] the gate **fails closed**: if the
//! provider is unreachable, the flow is denied. [`ComplianceMode::Monitor`]
//! records what would have been denied without blocking, which is the
//! intended setting while the product is still paper-only.
//!
//! Denials surface to clients as HTTP 451 with a generic message. The
//! specific reason (which list matched, the risk score) is kept in the
//! audit table and is never returned to the client, to avoid tipping off
//! the subject of a screening hit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// A flow that moves value and therefore must be screened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    Deposit,
    Withdrawal,
    OpenPosition,
    Settlement,
}

impl Flow {
    pub fn as_str(self) -> &'static str {
        match self {
            Flow::Deposit => "deposit",
            Flow::Withdrawal => "withdrawal",
            Flow::OpenPosition => "open_position",
            Flow::Settlement => "settlement",
        }
    }
}

/// How the gate treats a negative screening result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplianceMode {
    /// No screening at all. Only valid for local development.
    Off,
    /// Screen and record, but never block. Default while paper-only.
    Monitor,
    /// Screen, record and block. Required once funds move on-chain.
    Enforce,
}

impl ComplianceMode {
    /// Parses `ZENITH_COMPLIANCE_MODE`. Unknown values are rejected rather
    /// than silently falling back, so a typo cannot disable enforcement.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "monitor" => Ok(Self::Monitor),
            "enforce" => Ok(Self::Enforce),
            other => Err(format!(
                "invalid ZENITH_COMPLIANCE_MODE {other:?}: expected off, monitor or enforce"
            )),
        }
    }

    fn enforced(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// Screening verdict, ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Clear,
    /// Elevated risk: held for manual review. Denied under enforcement
    /// until an operator issues an allow override.
    Review,
    Blocked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Clear => "clear",
            Verdict::Review => "review",
            Verdict::Blocked => "blocked",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "clear" => Some(Self::Clear),
            "review" => Some(Self::Review),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

/// The result of screening one address with one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreeningResult {
    pub verdict: Verdict,
    /// 0 (no risk) to 100 (certain match).
    pub risk_score: u8,
    pub reasons: Vec<String>,
    pub provider: String,
    pub list_version: Option<String>,
}

impl ScreeningResult {
    pub fn clear(provider: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Clear,
            risk_score: 0,
            reasons: Vec::new(),
            provider: provider.into(),
            list_version: None,
        }
    }
}

#[derive(Debug)]
pub enum ScreeningError {
    /// The provider could not be reached or returned an error.
    Unavailable(String),
    /// The provider answered with something we could not interpret.
    InvalidResponse(String),
}

impl std::fmt::Display for ScreeningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "screening provider unavailable: {m}"),
            Self::InvalidResponse(m) => {
                write!(f, "screening provider returned invalid response: {m}")
            }
        }
    }
}

impl std::error::Error for ScreeningError {}

/// Answers whether an address is sanctioned or otherwise high risk.
#[async_trait]
pub trait ScreeningProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn screen(&self, wallet: &str) -> Result<ScreeningResult, ScreeningError>;
}

/// Canonical form used for every lookup and every stored row. Stellar
/// strkeys are upper-case base32, so case-folding never merges two
/// distinct addresses.
pub fn normalize_wallet(wallet: &str) -> String {
    wallet.trim().to_ascii_uppercase()
}

// ─── Providers ────────────────────────────────────────────────────────────────

/// Screens against a locally held list of sanctioned addresses.
///
/// The list format is JSON:
///
/// ```json
/// { "version": "ofac-sdn-2026-10-20",
///   "entries": [ { "address": "G...", "reason": "OFAC SDN: <program>" } ] }
/// ```
#[derive(Debug, Clone)]
pub struct DenylistProvider {
    version: String,
    entries: HashMap<String, String>,
}

#[derive(Deserialize)]
struct DenylistFile {
    version: String,
    entries: Vec<DenylistEntry>,
}

#[derive(Deserialize)]
struct DenylistEntry {
    address: String,
    reason: String,
}

impl DenylistProvider {
    pub fn new(
        version: impl Into<String>,
        entries: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            version: version.into(),
            entries: entries
                .into_iter()
                .map(|(address, reason)| (normalize_wallet(&address), reason))
                .collect(),
        }
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        let file: DenylistFile =
            serde_json::from_str(json).map_err(|e| format!("invalid denylist JSON: {e}"))?;
        if file.version.trim().is_empty() {
            return Err("denylist version must not be empty".into());
        }
        Ok(Self::new(
            file.version,
            file.entries.into_iter().map(|e| (e.address, e.reason)),
        ))
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read denylist {}: {e}", path.display()))?;
        Self::from_json(&json)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[async_trait]
impl ScreeningProvider for DenylistProvider {
    fn name(&self) -> &str {
        "denylist"
    }

    async fn screen(&self, wallet: &str) -> Result<ScreeningResult, ScreeningError> {
        let mut result = ScreeningResult::clear(self.name());
        result.list_version = Some(self.version.clone());
        if let Some(reason) = self.entries.get(&normalize_wallet(wallet)) {
            result.verdict = Verdict::Blocked;
            result.risk_score = 100;
            result.reasons.push(reason.clone());
        }
        Ok(result)
    }
}

/// Generic adapter for a vendor screening API (Chainalysis, TRM, Elliptic,
/// ... all expose an equivalent "screen this address" call).
///
/// Sends `POST {url}` with `{"address": "...", "network": "stellar"}` and
/// expects `{"risk_score": 0-100, "verdict": "clear|review|blocked",
/// "reasons": [...], "list_version": "..."}`. `verdict` is optional — when
/// absent it is derived from the score by the gate's thresholds. A vendor
/// with a different shape needs a thin proxy or its own provider impl.
pub struct HttpScreeningProvider {
    client: reqwest::Client,
    url: String,
    token: Option<SecretString>,
}

#[derive(Deserialize)]
struct HttpScreeningResponse {
    risk_score: u8,
    verdict: Option<Verdict>,
    #[serde(default)]
    reasons: Vec<String>,
    list_version: Option<String>,
}

impl HttpScreeningProvider {
    pub fn new(url: impl Into<String>, token: Option<SecretString>, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client");
        Self {
            client,
            url: url.into(),
            token,
        }
    }
}

#[async_trait]
impl ScreeningProvider for HttpScreeningProvider {
    fn name(&self) -> &str {
        "http"
    }

    async fn screen(&self, wallet: &str) -> Result<ScreeningResult, ScreeningError> {
        let mut request = self.client.post(&self.url).json(&serde_json::json!({
            "address": normalize_wallet(wallet),
            "network": "stellar",
        }));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose_secret());
        }
        let response = request
            .send()
            .await
            .map_err(|e| ScreeningError::Unavailable(e.to_string()))?;
        if !response.status().is_success() {
            return Err(ScreeningError::Unavailable(format!(
                "HTTP {}",
                response.status()
            )));
        }
        let body: HttpScreeningResponse = response
            .json()
            .await
            .map_err(|e| ScreeningError::InvalidResponse(e.to_string()))?;
        if body.risk_score > 100 {
            return Err(ScreeningError::InvalidResponse(format!(
                "risk_score {} out of range",
                body.risk_score
            )));
        }
        Ok(ScreeningResult {
            // Placeholder when the vendor gave no verdict; the gate
            // re-derives the effective verdict from the score anyway.
            verdict: body.verdict.unwrap_or(Verdict::Clear),
            risk_score: body.risk_score,
            reasons: body.reasons,
            provider: self.name().to_string(),
            list_version: body.list_version,
        })
    }
}

/// Runs several providers; the most severe verdict and highest score win.
/// Any provider error fails the whole screening — a partial answer is not
/// treated as clear.
pub struct CompositeProvider {
    name: String,
    providers: Vec<Arc<dyn ScreeningProvider>>,
}

impl CompositeProvider {
    pub fn new(providers: Vec<Arc<dyn ScreeningProvider>>) -> Self {
        let name = providers
            .iter()
            .map(|p| p.name().to_string())
            .collect::<Vec<_>>()
            .join("+");
        Self { name, providers }
    }
}

#[async_trait]
impl ScreeningProvider for CompositeProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn screen(&self, wallet: &str) -> Result<ScreeningResult, ScreeningError> {
        let mut combined = ScreeningResult::clear(self.name.clone());
        let mut versions = Vec::new();
        for provider in &self.providers {
            let r = provider.screen(wallet).await?;
            combined.verdict = combined.verdict.max(r.verdict);
            combined.risk_score = combined.risk_score.max(r.risk_score);
            combined.reasons.extend(
                r.reasons
                    .into_iter()
                    .map(|m| format!("{}: {m}", r.provider)),
            );
            if let Some(v) = r.list_version {
                versions.push(format!("{}={v}", r.provider));
            }
        }
        if !versions.is_empty() {
            combined.list_version = Some(versions.join(","));
        }
        Ok(combined)
    }
}

/// Builds the provider from the environment: `ZENITH_SANCTIONS_LIST_PATH`
/// (local denylist) and/or `ZENITH_SCREENING_URL` + `ZENITH_SCREENING_TOKEN`
/// (vendor API). When both are set they are combined. When neither is set
/// this is an error: there is no implicit "screen nothing" provider.
pub fn provider_from_env() -> Result<Arc<dyn ScreeningProvider>, String> {
    let mut providers: Vec<Arc<dyn ScreeningProvider>> = Vec::new();
    if let Ok(path) = std::env::var("ZENITH_SANCTIONS_LIST_PATH") {
        providers.push(Arc::new(DenylistProvider::from_file(
            std::path::Path::new(&path),
        )?));
    }
    if let Ok(url) = std::env::var("ZENITH_SCREENING_URL") {
        let token = std::env::var("ZENITH_SCREENING_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(SecretString::new);
        providers.push(Arc::new(HttpScreeningProvider::new(
            url,
            token,
            Duration::from_secs(5),
        )));
    }
    match providers.len() {
        0 => Err("no screening provider configured: set ZENITH_SANCTIONS_LIST_PATH and/or ZENITH_SCREENING_URL".into()),
        1 => Ok(providers.pop().expect("one provider")),
        _ => Ok(Arc::new(CompositeProvider::new(providers))),
    }
}

// ─── Gate ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CompliancePolicy {
    pub mode: ComplianceMode,
    /// Scores at or above this are treated as at least `Review`.
    pub review_score: u8,
    /// Scores at or above this are treated as `Blocked`.
    pub block_score: u8,
    /// How long a screening result is reused before re-screening.
    pub cache_ttl_secs: i64,
}

impl Default for CompliancePolicy {
    fn default() -> Self {
        Self {
            mode: ComplianceMode::Monitor,
            review_score: 50,
            block_score: 80,
            cache_ttl_secs: 24 * 60 * 60,
        }
    }
}

impl CompliancePolicy {
    /// Reads `ZENITH_COMPLIANCE_MODE`, `ZENITH_COMPLIANCE_REVIEW_SCORE`,
    /// `ZENITH_COMPLIANCE_BLOCK_SCORE` and `ZENITH_COMPLIANCE_CACHE_TTL_SECS`.
    pub fn from_env() -> Result<Self, String> {
        let mut policy = Self::default();
        if let Ok(mode) = std::env::var("ZENITH_COMPLIANCE_MODE") {
            policy.mode = ComplianceMode::parse(&mode)?;
        }
        let parse_u8 = |key: &str, default: u8| -> Result<u8, String> {
            match std::env::var(key) {
                Ok(v) => v
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|n| *n <= 100)
                    .ok_or_else(|| format!("{key} must be an integer 0-100")),
                Err(_) => Ok(default),
            }
        };
        policy.review_score = parse_u8("ZENITH_COMPLIANCE_REVIEW_SCORE", policy.review_score)?;
        policy.block_score = parse_u8("ZENITH_COMPLIANCE_BLOCK_SCORE", policy.block_score)?;
        if let Ok(v) = std::env::var("ZENITH_COMPLIANCE_CACHE_TTL_SECS") {
            policy.cache_ttl_secs = v
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .ok_or("ZENITH_COMPLIANCE_CACHE_TTL_SECS must be a non-negative integer")?;
        }
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.review_score > self.block_score {
            return Err("review score threshold must not exceed block threshold".into());
        }
        Ok(())
    }

    /// The effective verdict: the provider's verdict, escalated by score.
    fn effective_verdict(&self, result: &ScreeningResult) -> Verdict {
        let by_score = if result.risk_score >= self.block_score {
            Verdict::Blocked
        } else if result.risk_score >= self.review_score {
            Verdict::Review
        } else {
            Verdict::Clear
        };
        result.verdict.max(by_score)
    }
}

/// What the gate decided for one wallet and flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    /// False when the gate is in monitor mode and a deny was only recorded.
    pub enforced: bool,
    pub verdict: Option<Verdict>,
    pub source: DecisionSource,
    /// Internal reason. Never return this to the client.
    pub reason: String,
    pub decision_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    Screening,
    Cache,
    Override,
    Unavailable,
    Disabled,
}

impl DecisionSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Screening => "screening",
            Self::Cache => "cache",
            Self::Override => "override",
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug)]
pub enum ComplianceError {
    /// The flow is not permitted for this wallet.
    Denied {
        decision_id: Option<i64>,
    },
    Db(sqlx::Error),
    InvalidRequest(String),
}

impl std::fmt::Display for ComplianceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied { .. } => write!(f, "compliance check denied this action"),
            Self::Db(e) => write!(f, "compliance database error: {e}"),
            Self::InvalidRequest(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ComplianceError {}

impl From<sqlx::Error> for ComplianceError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl IntoResponse for ComplianceError {
    fn into_response(self) -> Response {
        match self {
            // 451: the request is refused for legal reasons. Generic message
            // on purpose — the reason lives in the audit table only.
            Self::Denied { .. } => (
                StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS,
                Json(serde_json::json!({
                    "error": "this action is not available for this account",
                    "code": "compliance_denied",
                })),
            )
                .into_response(),
            Self::InvalidRequest(m) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": m, "code": "invalid_request" })),
            )
                .into_response(),
            Self::Db(e) => {
                tracing::error!(error = %e, "compliance gate database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "internal error", "code": "internal" })),
                )
                    .into_response()
            }
        }
    }
}

/// Raw `compliance_overrides` row as selected by [`ComplianceGate::active_override`].
type OverrideRow = (i64, String, String, String, String, i64, Option<i64>);

/// An operator override as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub id: i64,
    pub wallet_address: String,
    pub allow: bool,
    pub reason: String,
    pub operator: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

/// Screens wallets before value moves. Cheap to clone.
#[derive(Clone)]
pub struct ComplianceGate {
    pool: SqlitePool,
    provider: Arc<dyn ScreeningProvider>,
    policy: CompliancePolicy,
}

impl ComplianceGate {
    pub fn new(
        pool: SqlitePool,
        provider: Arc<dyn ScreeningProvider>,
        policy: CompliancePolicy,
    ) -> Self {
        Self {
            pool,
            provider,
            policy,
        }
    }

    pub fn policy(&self) -> &CompliancePolicy {
        &self.policy
    }

    /// Screens `wallet` for `flow` and records the decision. `now` is Unix
    /// seconds; it is a parameter so expiry and TTL are testable.
    pub async fn check(
        &self,
        wallet: &str,
        flow: Flow,
        now: i64,
    ) -> Result<Decision, ComplianceError> {
        let wallet = normalize_wallet(wallet);
        if wallet.is_empty() {
            return Err(ComplianceError::InvalidRequest(
                "wallet address is required".into(),
            ));
        }
        let mode = self.policy.mode;
        if mode == ComplianceMode::Off {
            return Ok(Decision {
                allowed: true,
                enforced: false,
                verdict: None,
                source: DecisionSource::Disabled,
                reason: "compliance screening disabled".into(),
                decision_id: None,
            });
        }

        // 1. Operator overrides win over any screening result.
        if let Some(ov) = self.active_override(&wallet, now).await? {
            let reason = format!(
                "{} override #{} by {}: {}",
                if ov.allow { "allow" } else { "deny" },
                ov.id,
                ov.operator,
                ov.reason
            );
            return self
                .record(
                    &wallet,
                    flow,
                    ov.allow,
                    None,
                    DecisionSource::Override,
                    reason,
                    None,
                    Some(ov.id),
                    now,
                )
                .await;
        }

        // 2. A fresh cached screening, else 3. ask the provider.
        let (screening_id, result, source) = match self.cached_screening(&wallet, now).await? {
            Some((id, result)) => (id, result, DecisionSource::Cache),
            None => match self.provider.screen(&wallet).await {
                Ok(result) => {
                    let id = self.store_screening(&wallet, &result, now).await?;
                    (id, result, DecisionSource::Screening)
                }
                Err(e) => {
                    // Fail closed: no answer is not a clear answer.
                    tracing::warn!(wallet = %wallet, flow = flow.as_str(), error = %e, "compliance screening unavailable");
                    return self
                        .record(
                            &wallet,
                            flow,
                            false,
                            None,
                            DecisionSource::Unavailable,
                            e.to_string(),
                            None,
                            None,
                            now,
                        )
                        .await;
                }
            },
        };

        let verdict = self.policy.effective_verdict(&result);
        let reason = if result.reasons.is_empty() {
            format!("{} (score {})", verdict.as_str(), result.risk_score)
        } else {
            format!(
                "{} (score {}): {}",
                verdict.as_str(),
                result.risk_score,
                result.reasons.join("; ")
            )
        };
        self.record(
            &wallet,
            flow,
            verdict == Verdict::Clear,
            Some(verdict),
            source,
            reason,
            Some(screening_id),
            None,
            now,
        )
        .await
    }

    /// Like [`check`](Self::check), but returns `Err(Denied)` when the flow
    /// must not proceed. Use this at the top of every value-moving handler.
    pub async fn require(
        &self,
        wallet: &str,
        flow: Flow,
        now: i64,
    ) -> Result<Decision, ComplianceError> {
        let decision = self.check(wallet, flow, now).await?;
        if decision.allowed || !decision.enforced {
            Ok(decision)
        } else {
            Err(ComplianceError::Denied {
                decision_id: decision.decision_id,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        wallet: &str,
        flow: Flow,
        allowed: bool,
        verdict: Option<Verdict>,
        source: DecisionSource,
        reason: String,
        screening_id: Option<i64>,
        override_id: Option<i64>,
        now: i64,
    ) -> Result<Decision, ComplianceError> {
        let enforced = self.policy.mode.enforced();
        if !allowed && !enforced {
            tracing::warn!(wallet, flow = flow.as_str(), reason = %reason, "compliance would deny (monitor mode)");
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO compliance_decisions \
                 (wallet_address, flow, outcome, enforced, source, verdict, reason, screening_id, override_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(wallet)
        .bind(flow.as_str())
        .bind(if allowed { "allow" } else { "deny" })
        .bind(enforced as i64)
        .bind(source.as_str())
        .bind(verdict.map(Verdict::as_str))
        .bind(&reason)
        .bind(screening_id)
        .bind(override_id)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(Decision {
            allowed,
            enforced,
            verdict,
            source,
            reason,
            decision_id: Some(id),
        })
    }

    async fn cached_screening(
        &self,
        wallet: &str,
        now: i64,
    ) -> Result<Option<(i64, ScreeningResult)>, ComplianceError> {
        let row: Option<(i64, String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, verdict, risk_score, reasons, provider, list_version \
             FROM compliance_screenings \
             WHERE wallet_address = ? AND provider = ? AND screened_at > ? \
             ORDER BY screened_at DESC, id DESC LIMIT 1",
        )
        .bind(wallet)
        .bind(self.provider.name())
        .bind(now - self.policy.cache_ttl_secs)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, verdict, score, reasons, provider, list_version)) = row else {
            return Ok(None);
        };
        // An unreadable cached row is ignored (forcing a fresh screening)
        // rather than trusted.
        let Some(verdict) = Verdict::parse(&verdict) else {
            return Ok(None);
        };
        let reasons: Vec<String> = serde_json::from_str(&reasons).unwrap_or_default();
        Ok(Some((
            id,
            ScreeningResult {
                verdict,
                risk_score: score.clamp(0, 100) as u8,
                reasons,
                provider,
                list_version,
            },
        )))
    }

    async fn store_screening(
        &self,
        wallet: &str,
        r: &ScreeningResult,
        now: i64,
    ) -> Result<i64, ComplianceError> {
        let reasons = serde_json::to_string(&r.reasons).expect("reasons serialize");
        Ok(sqlx::query_scalar(
            "INSERT INTO compliance_screenings \
                 (wallet_address, provider, verdict, risk_score, reasons, list_version, screened_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(wallet)
        // Keyed by the gate's provider name so the cache lookup matches.
        .bind(self.provider.name())
        .bind(r.verdict.as_str())
        .bind(r.risk_score as i64)
        .bind(reasons)
        .bind(&r.list_version)
        .bind(now)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The most recent unexpired, unrevoked override for `wallet`, if any.
    pub async fn active_override(
        &self,
        wallet: &str,
        now: i64,
    ) -> Result<Option<Override>, ComplianceError> {
        let row: Option<OverrideRow> = sqlx::query_as(
            "SELECT id, wallet_address, action, reason, operator, created_at, expires_at \
             FROM compliance_overrides \
             WHERE wallet_address = ? AND revoked_at IS NULL \
               AND (expires_at IS NULL OR expires_at > ?) \
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(normalize_wallet(wallet))
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(id, wallet_address, action, reason, operator, created_at, expires_at)| Override {
                id,
                wallet_address,
                allow: action == "allow",
                reason,
                operator,
                created_at,
                expires_at,
            },
        ))
    }

    /// Issues an operator override. A reason and operator are mandatory so
    /// every manual exception is attributable.
    pub async fn set_override(
        &self,
        wallet: &str,
        allow: bool,
        operator: &str,
        reason: &str,
        expires_at: Option<i64>,
        now: i64,
    ) -> Result<i64, ComplianceError> {
        if operator.trim().is_empty() || reason.trim().is_empty() {
            return Err(ComplianceError::InvalidRequest(
                "override requires an operator and a reason".into(),
            ));
        }
        if matches!(expires_at, Some(t) if t <= now) {
            return Err(ComplianceError::InvalidRequest(
                "override expiry must be in the future".into(),
            ));
        }
        Ok(sqlx::query_scalar(
            "INSERT INTO compliance_overrides \
                 (wallet_address, action, reason, operator, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(normalize_wallet(wallet))
        .bind(if allow { "allow" } else { "deny" })
        .bind(reason.trim())
        .bind(operator.trim())
        .bind(now)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn revoke_override(
        &self,
        override_id: i64,
        operator: &str,
        now: i64,
    ) -> Result<bool, ComplianceError> {
        let done = sqlx::query(
            "UPDATE compliance_overrides SET revoked_at = ?, revoked_by = ? \
             WHERE id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(operator)
        .bind(override_id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SANCTIONED: &str = "GSANCTIONEDAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const CLEAN: &str = "GCLEANAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0012_compliance_screening.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    fn denylist() -> Arc<dyn ScreeningProvider> {
        Arc::new(DenylistProvider::new(
            "test-list-1",
            [(SANCTIONED.to_string(), "OFAC SDN test entry".to_string())],
        ))
    }

    fn policy(mode: ComplianceMode) -> CompliancePolicy {
        CompliancePolicy {
            mode,
            ..CompliancePolicy::default()
        }
    }

    struct Counting {
        calls: AtomicUsize,
        result: Result<ScreeningResult, ()>,
    }

    #[async_trait]
    impl ScreeningProvider for Counting {
        fn name(&self) -> &str {
            "counting"
        }
        async fn screen(&self, _wallet: &str) -> Result<ScreeningResult, ScreeningError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result
                .clone()
                .map_err(|_| ScreeningError::Unavailable("down".into()))
        }
    }

    #[tokio::test]
    async fn enforce_blocks_sanctioned_and_allows_clean() {
        let gate = ComplianceGate::new(pool().await, denylist(), policy(ComplianceMode::Enforce));
        let denied = gate.require(SANCTIONED, Flow::Withdrawal, 1_000).await;
        assert!(matches!(denied, Err(ComplianceError::Denied { .. })));
        // Case/whitespace variants of the address are the same wallet.
        let lower = format!("  {}  ", SANCTIONED.to_lowercase());
        assert!(gate.require(&lower, Flow::Deposit, 1_000).await.is_err());

        let ok = gate.require(CLEAN, Flow::Deposit, 1_000).await.unwrap();
        assert!(ok.allowed);
        assert_eq!(ok.verdict, Some(Verdict::Clear));
    }

    #[tokio::test]
    async fn monitor_records_but_does_not_block() {
        let pool = pool().await;
        let gate = ComplianceGate::new(pool.clone(), denylist(), policy(ComplianceMode::Monitor));
        let d = gate
            .require(SANCTIONED, Flow::Settlement, 1_000)
            .await
            .unwrap();
        assert!(!d.allowed);
        assert!(!d.enforced);
        let (outcome, enforced): (String, i64) =
            sqlx::query_as("SELECT outcome, enforced FROM compliance_decisions WHERE id = ?")
                .bind(d.decision_id.unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((outcome.as_str(), enforced), ("deny", 0));
    }

    #[tokio::test]
    async fn provider_outage_fails_closed_under_enforcement() {
        let provider = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            result: Err(()),
        });
        let gate = ComplianceGate::new(pool().await, provider, policy(ComplianceMode::Enforce));
        let d = gate.check(CLEAN, Flow::Deposit, 1_000).await.unwrap();
        assert!(!d.allowed && d.enforced);
        assert_eq!(d.source, DecisionSource::Unavailable);
        assert!(gate.require(CLEAN, Flow::Deposit, 1_000).await.is_err());
    }

    #[tokio::test]
    async fn screenings_are_cached_until_ttl() {
        let provider = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            result: Ok(ScreeningResult::clear("counting")),
        });
        let mut p = policy(ComplianceMode::Enforce);
        p.cache_ttl_secs = 100;
        let gate = ComplianceGate::new(pool().await, provider.clone(), p);

        assert_eq!(
            gate.check(CLEAN, Flow::Deposit, 1_000)
                .await
                .unwrap()
                .source,
            DecisionSource::Screening
        );
        assert_eq!(
            gate.check(CLEAN, Flow::Deposit, 1_050)
                .await
                .unwrap()
                .source,
            DecisionSource::Cache
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        // Past the TTL the wallet is re-screened.
        assert_eq!(
            gate.check(CLEAN, Flow::Deposit, 1_100)
                .await
                .unwrap()
                .source,
            DecisionSource::Screening
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn score_thresholds_escalate_the_verdict() {
        let mut result = ScreeningResult::clear("counting");
        result.risk_score = 60;
        let provider = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            result: Ok(result),
        });
        let gate = ComplianceGate::new(pool().await, provider, policy(ComplianceMode::Enforce));
        let d = gate.check(CLEAN, Flow::OpenPosition, 1_000).await.unwrap();
        assert_eq!(d.verdict, Some(Verdict::Review));
        assert!(!d.allowed, "review is held until an operator allows it");
    }

    #[tokio::test]
    async fn overrides_take_precedence_and_expire() {
        let gate = ComplianceGate::new(pool().await, denylist(), policy(ComplianceMode::Enforce));
        // False positive: allow until t=2000.
        let id = gate
            .set_override(
                SANCTIONED,
                true,
                "ops@zenith",
                "confirmed false positive, ticket #1",
                Some(2_000),
                1_000,
            )
            .await
            .unwrap();
        let d = gate
            .require(SANCTIONED, Flow::Withdrawal, 1_500)
            .await
            .unwrap();
        assert_eq!(d.source, DecisionSource::Override);
        // Expired: back to the screening result.
        assert!(gate
            .require(SANCTIONED, Flow::Withdrawal, 2_000)
            .await
            .is_err());

        // Manual freeze of a clean wallet, then revocation.
        let freeze = gate
            .set_override(
                CLEAN,
                false,
                "ops@zenith",
                "law-enforcement request",
                None,
                1_000,
            )
            .await
            .unwrap();
        assert!(gate.require(CLEAN, Flow::Withdrawal, 1_100).await.is_err());
        assert!(gate
            .revoke_override(freeze, "ops@zenith", 1_200)
            .await
            .unwrap());
        assert!(gate.require(CLEAN, Flow::Withdrawal, 1_300).await.is_ok());
        assert_ne!(id, freeze);
    }

    #[tokio::test]
    async fn overrides_require_operator_and_reason() {
        let gate = ComplianceGate::new(pool().await, denylist(), policy(ComplianceMode::Enforce));
        assert!(gate
            .set_override(CLEAN, true, "", "why", None, 1)
            .await
            .is_err());
        assert!(gate
            .set_override(CLEAN, true, "ops", "  ", None, 1)
            .await
            .is_err());
        assert!(gate
            .set_override(CLEAN, true, "ops", "why", Some(1), 1)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn decision_log_is_append_only() {
        let pool = pool().await;
        let gate = ComplianceGate::new(pool.clone(), denylist(), policy(ComplianceMode::Enforce));
        gate.check(SANCTIONED, Flow::Deposit, 1).await.unwrap();
        assert!(
            sqlx::query("UPDATE compliance_decisions SET outcome = 'allow'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(sqlx::query("DELETE FROM compliance_decisions")
            .execute(&pool)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn off_mode_skips_screening_entirely() {
        let pool = pool().await;
        let gate = ComplianceGate::new(pool.clone(), denylist(), policy(ComplianceMode::Off));
        assert!(
            gate.require(SANCTIONED, Flow::Deposit, 1)
                .await
                .unwrap()
                .allowed
        );
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM compliance_decisions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn composite_takes_worst_verdict_and_propagates_errors() {
        let clean: Arc<dyn ScreeningProvider> = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            result: Ok(ScreeningResult::clear("counting")),
        });
        let combined = CompositeProvider::new(vec![clean.clone(), denylist()]);
        let r = combined.screen(SANCTIONED).await.unwrap();
        assert_eq!(r.verdict, Verdict::Blocked);
        assert_eq!(r.list_version.as_deref(), Some("denylist=test-list-1"));

        let down: Arc<dyn ScreeningProvider> = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            result: Err(()),
        });
        assert!(CompositeProvider::new(vec![clean, down])
            .screen(CLEAN)
            .await
            .is_err());
    }

    #[test]
    fn denylist_json_and_mode_parsing() {
        let list = DenylistProvider::from_json(
            r#"{"version":"v1","entries":[{"address":"gabc","reason":"r"}]}"#,
        )
        .unwrap();
        assert_eq!(list.len(), 1);
        assert!(DenylistProvider::from_json(r#"{"version":" ","entries":[]}"#).is_err());
        assert_eq!(
            ComplianceMode::parse(" Enforce ").unwrap(),
            ComplianceMode::Enforce
        );
        assert!(ComplianceMode::parse("enforcing").is_err());
    }

    #[tokio::test]
    async fn http_provider_parses_vendor_response_and_sends_token() {
        use axum::{http::HeaderMap, routing::post, Router};
        async fn handler(
            headers: HeaderMap,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            assert_eq!(headers.get("authorization").unwrap(), "Bearer t0ken");
            let risky = body["address"].as_str().unwrap().starts_with("GSANCTIONED");
            Json(serde_json::json!({
                "risk_score": if risky { 95 } else { 3 },
                "reasons": if risky { vec!["sanctions"] } else { vec![] },
                "list_version": "vendor-7",
            }))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/screen", post(handler)))
                .await
                .unwrap()
        });

        let provider = Arc::new(HttpScreeningProvider::new(
            format!("http://{addr}/screen"),
            Some(SecretString::new("t0ken".into())),
            Duration::from_secs(5),
        ));
        let gate = ComplianceGate::new(
            pool().await,
            provider.clone(),
            policy(ComplianceMode::Enforce),
        );
        assert!(gate.require(CLEAN, Flow::Deposit, 1).await.is_ok());
        let d = gate.check(SANCTIONED, Flow::Deposit, 1).await.unwrap();
        assert_eq!(
            d.verdict,
            Some(Verdict::Blocked),
            "score 95 >= block threshold"
        );

        let unreachable = HttpScreeningProvider::new(
            "http://127.0.0.1:1/screen",
            None,
            Duration::from_millis(500),
        );
        assert!(matches!(
            unreachable.screen(CLEAN).await,
            Err(ScreeningError::Unavailable(_))
        ));
    }
}
