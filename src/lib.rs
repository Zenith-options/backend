use axum::http::Method;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::{f64::consts::PI, sync::Arc};
use tower_http::cors::{Any, CorsLayer};
use tower_http::request_id::{PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

pub mod admin;
pub mod chain;
pub mod expiry;
pub mod margin;
pub mod pagination;
pub mod alerts;
pub mod abuse;
pub mod api_version;
pub mod audit;
pub mod auth;
pub mod chain_cost;
pub mod collateral;
pub mod db;
pub mod error;
pub mod history;
pub mod governance;
pub mod indexer;
pub mod keeper;
pub mod math_fixed;
pub mod margin_fixed;
pub mod models;
pub mod operator;
pub mod outbox;
pub mod pagination;
pub mod partial_exercise;
pub mod payoff;
pub mod preview;
pub mod privacy;
pub mod series_lifecycle;
pub mod settlement_policy;
pub mod settlement_reports;
pub mod positions;
pub mod protocol_params;
pub mod public_api;
pub mod prices;
pub mod rate_limit_key;
pub mod rewards;
pub mod request_id;
pub mod risk_stress;
pub mod secrets;
pub mod security_policy;
pub mod settlement_notifications;
pub mod settlement_price;
pub mod settlement_saga;
pub mod signing;
pub mod soroban;
pub mod strategies;
pub mod treasury;
pub mod strkey;
pub mod watchlist;
pub mod voting_power;

// Kani proof harnesses for the fixed-point arithmetic in `math_fixed`
// (issue #119). Only compiled under `cargo kani`, so normal builds and
// tests never pay for it — see `docs/verification.md`.
#[cfg(kani)]
mod kani_proofs;

use error::AppQuery;

// ─── Black-Scholes Pricing Engine ─────────────────────────────────────────────

/// Cumulative standard normal distribution (Abramowitz & Stegun approximation)
fn norm_cdf(x: f64) -> f64 {
    if x < -7.0 {
        return 0.0;
    }
    if x > 7.0 {
        return 1.0;
    }
    let k = 1.0 / (1.0 + 0.2316419 * x.abs());
    let poly = k
        * (0.319381530
            + k * (-0.356563782 + k * (1.781477937 + k * (-1.821255978 + k * 1.330274429))));
    let pdf = (-x * x / 2.0).exp() / (2.0 * PI).sqrt();
    if x >= 0.0 {
        1.0 - pdf * poly
    } else {
        pdf * poly
    }
}

/// Standard normal PDF
fn norm_pdf(x: f64) -> f64 {
    (-x * x / 2.0).exp() / (2.0 * PI).sqrt()
}

/// Realistic crypto vol smile: left (put) skew, curvature, wing term.
/// Ported bit-for-bit from the frontend's lib/pricing.ts smileVol() so
/// /api/v1/price and /api/v1/chain price consistently with what the client
/// already shows — this previously used one flat vol per underlying for
/// every strike. Note the wing term is `(|m|-0.15)^2` unconditionally: the
/// frontend wraps it in `Math.max(0, ...)`, but a square can't be negative,
/// so that outer clamp is a no-op there and is reproduced as a no-op here
/// too, on purpose, for numeric parity rather than "fixing" a shipped quirk
/// unilaterally on just one side.
pub fn smile_vol(base: f64, moneyness: f64) -> f64 {
    let m = moneyness - 1.0;
    let wing = (m.abs() - 0.15).powi(2);
    (base - 0.15 * m + 0.08 * m * m + 0.12 * wing).max(0.1)
}

#[derive(Debug, Clone)]
pub struct BSInputs {
    pub spot: f64,   // current price
    pub strike: f64, // strike price
    pub vol: f64,    // annualised volatility (e.g. 0.80 = 80%)
    pub t: f64,      // time to expiry in years
    pub r: f64,      // risk-free rate (e.g. 0.05 = 5%)
    pub is_call: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BSResult {
    pub premium: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64, // per day
    pub vega: f64,  // per 1% vol move
    pub rho: f64,
    pub d1: f64,
    pub d2: f64,
    pub intrinsic: f64,
    pub time_value: f64,
    pub iv: f64,
}

pub fn black_scholes(inputs: &BSInputs) -> BSResult {
    let BSInputs {
        spot: s,
        strike: k,
        vol: sigma,
        t,
        r,
        is_call,
    } = *inputs;

    if t <= 0.0 {
        // At expiry: intrinsic only
        let intrinsic = if is_call {
            (s - k).max(0.0)
        } else {
            (k - s).max(0.0)
        };
        return BSResult {
            premium: intrinsic,
            delta: if is_call { 1.0 } else { -1.0 },
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            rho: 0.0,
            d1: 0.0,
            d2: 0.0,
            intrinsic,
            time_value: 0.0,
            iv: sigma,
        };
    }

    let sqrt_t = t.sqrt();
    let d1 = ((s / k).ln() + (r + 0.5 * sigma * sigma) * t) / (sigma * sqrt_t);
    let d2 = d1 - sigma * sqrt_t;

    let disc = (-r * t).exp();

    let (premium, delta, rho) = if is_call {
        let nd1 = norm_cdf(d1);
        let nd2 = norm_cdf(d2);
        let price = s * nd1 - k * disc * nd2;
        let del = nd1;
        let r_val = k * t * disc * nd2 / 100.0;
        (price, del, r_val)
    } else {
        let nd1 = norm_cdf(-d1);
        let nd2 = norm_cdf(-d2);
        let price = k * disc * nd2 - s * nd1;
        let del = nd1 - 1.0;
        let r_val = -k * t * disc * nd2 / 100.0;
        (price, del, r_val)
    };

    let pdf_d1 = norm_pdf(d1);
    let gamma = pdf_d1 / (s * sigma * sqrt_t);
    let vega = s * pdf_d1 * sqrt_t / 100.0; // per 1% vol

    let theta = if is_call {
        (-(s * pdf_d1 * sigma) / (2.0 * sqrt_t) - r * k * disc * norm_cdf(d2)) / 365.0
    } else {
        (-(s * pdf_d1 * sigma) / (2.0 * sqrt_t) + r * k * disc * norm_cdf(-d2)) / 365.0
    };

    let intrinsic = if is_call {
        (s - k).max(0.0)
    } else {
        (k - s).max(0.0)
    };
    let time_value = premium - intrinsic;

    BSResult {
        premium,
        delta,
        gamma,
        theta,
        vega,
        rho,
        d1,
        d2,
        intrinsic,
        time_value,
        iv: sigma,
    }
}

/// Newton-Raphson implied volatility solver
pub fn implied_vol(
    market_price: f64,
    spot: f64,
    strike: f64,
    t: f64,
    r: f64,
    is_call: bool,
) -> Option<f64> {
    let intrinsic = if is_call {
        (spot - strike).max(0.0)
    } else {
        (strike - spot).max(0.0)
    };
    if market_price < intrinsic {
        return None;
    }

    let mut sigma = 0.5_f64; // initial guess
    for _ in 0..100 {
        let bs = black_scholes(&BSInputs {
            spot,
            strike,
            vol: sigma,
            t,
            r,
            is_call,
        });
        let diff = bs.premium - market_price;
        if diff.abs() < 1e-6 {
            return Some(sigma);
        }
        if bs.vega.abs() < 1e-10 {
            break;
        }
        sigma -= diff / (bs.vega * 100.0); // vega is per 1%, need per unit
        sigma = sigma.clamp(0.001, 10.0);
    }
    Some(sigma)
}

// ─── Market Data ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub spot_prices: Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
    pub vol_surface: Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
    pub db: sqlx::SqlitePool,
    pub cursor_secret: String,
    /// Broadcasts a JSON-encoded SpotResponse every time the price
    /// simulator nudges spot_prices, for the /api/v1/ws/spot handler to
    /// forward to connected clients. `send` errors (no receivers) are
    /// expected and ignored — the simulator runs regardless of whether
    /// anyone's listening.
    pub spot_tx: tokio::sync::broadcast::Sender<String>,
    /// HMAC key used to sign and verify opaque keyset-pagination cursors.
    /// Loaded from `ZENITH_CURSOR_SECRET` when set; otherwise a fixed
    /// development default is used so local runs and tests work without
    /// configuration. Production deployments must set the env var.
    pub cursor_secret: Arc<Vec<u8>>,
}

impl AppState {
    pub fn new(db: sqlx::SqlitePool) -> Self {
        let mut prices = std::collections::HashMap::new();
        prices.insert("XLM".into(), 0.1182);
        prices.insert("BTC".into(), 67420.50);
        prices.insert("ETH".into(), 3512.80);
        prices.insert("SOL".into(), 182.45);

        let mut vols = std::collections::HashMap::new();
        vols.insert("XLM".into(), 0.82); // 82% ann vol
        vols.insert("BTC".into(), 0.65);
        vols.insert("ETH".into(), 0.72);
        vols.insert("SOL".into(), 0.91);

        let (spot_tx, _) = tokio::sync::broadcast::channel(16);

        let cursor_secret = std::env::var("ZENITH_CURSOR_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| b"zenith-dev-cursor-secret".to_vec());

        Self {
            spot_prices: Arc::new(std::sync::Mutex::new(prices)),
            vol_surface: Arc::new(std::sync::Mutex::new(vols)),
            db,
            cursor_secret: std::env::var("CURSOR_SECRET").unwrap_or_else(|_| "zenith-cursor-secret".to_string()),
            spot_tx,
            cursor_secret: Arc::new(cursor_secret),
        }
    }
}

// ─── Request / Response Types ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PriceQuery {
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String, // "call" | "put"
}

#[derive(Deserialize)]
pub struct IvQuery {
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String, // "call" | "put"
    pub market_price: f64,
}

#[derive(Serialize)]
pub struct IvResult {
    pub implied_vol: f64,
}

#[derive(Serialize)]
pub struct OptionChainEntry {
    pub strike: f64,
    pub expiry_days: f64,
    pub call: BSResult,
    pub put: BSResult,
    pub is_itm_call: bool,
    pub is_itm_put: bool,
}

#[derive(Deserialize)]
pub struct ChainQuery {
    pub underlying: String,
    pub expiry_days: f64,
}

#[derive(Serialize)]
pub struct ExpiryCalendar {
    pub underlying: String,
    pub spot: f64,
    pub vol: f64,
    pub expiries: Vec<ExpiryInfo>,
}

#[derive(Serialize)]
pub struct ExpiryInfo {
    pub days_to_expiry: u32,
    pub label: String,
    pub timestamp: u64,
}

#[derive(Serialize)]
pub struct SpotResponse {
    pub prices: std::collections::HashMap<String, f64>,
    pub vols: std::collections::HashMap<String, f64>,
}

// ─── Route Handlers ───────────────────────────────────────────────────────────

/// Pings the database as part of the health check — a load balancer or
/// orchestrator should see this fail (and stop routing traffic here) if
/// the pool is exhausted or the file's gone missing, not just get a
/// hollow "ok" that only proves the HTTP server itself is up.
async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, error::AppError> {
    sqlx::query("SELECT 1")
        .execute(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "health check: database unavailable");
            error::AppError::new(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        })?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "service": "zenith-backend",
        "version": "0.1.0",
        "network": "stellar-testnet",
        "database": "ok"
    })))
}

async fn get_spot(State(state): State<AppState>) -> Json<SpotResponse> {
    let prices = state.spot_prices.lock().unwrap().clone();
    let vols = state.vol_surface.lock().unwrap().clone();
    Json(SpotResponse { prices, vols })
}

async fn price_option(
    State(state): State<AppState>,
    AppQuery(q): AppQuery<PriceQuery>,
) -> Result<Json<BSResult>, StatusCode> {
    let prices = state.spot_prices.lock().unwrap();
    let vols = state.vol_surface.lock().unwrap();

    let spot = *prices.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;
    let base_vol = *vols.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;
    let vol = smile_vol(base_vol, q.strike / spot);

    let t = q.expiry_days / 365.0;
    let is_call = q.option_type.to_lowercase() == "call";

    let result = black_scholes(&BSInputs {
        spot,
        strike: q.strike,
        vol,
        t,
        r: 0.05,
        is_call,
    });
    Ok(Json(result))
}

async fn get_chain(
    State(state): State<AppState>,
    AppQuery(q): AppQuery<ChainQuery>,
) -> Result<Json<Vec<OptionChainEntry>>, StatusCode> {
    let prices = state.spot_prices.lock().unwrap();
    let vols = state.vol_surface.lock().unwrap();

    let spot = *prices.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;
    let base_vol = *vols.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;
    drop(vols);
    drop(prices);
    let t = q.expiry_days / 365.0;
    let r = 0.05_f64;

    let matching_series: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM option_series
         WHERE UPPER(underlying) = UPPER(?1)
           AND ABS(julianday(expiry) - julianday('now') - ?2) <= 2",
    )
    .bind(&q.underlying)
    .bind(q.expiry_days)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, underlying = %q.underlying, "failed to filter chain by series lifecycle");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let strikes = if matching_series > 0 {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT strike_scaled, strike_scale FROM option_series
             WHERE UPPER(underlying) = UPPER(?1)
               AND ABS(julianday(expiry) - julianday('now') - ?2) <= 2
             GROUP BY strike_scaled, strike_scale
             HAVING SUM(CASE WHEN status = 'active' AND trading_halted = 0
                                  AND option_type = 'call' THEN 1 ELSE 0 END) > 0
                AND SUM(CASE WHEN status = 'active' AND trading_halted = 0
                                  AND option_type = 'put' THEN 1 ELSE 0 END) > 0
             ORDER BY strike_scaled",
        )
        .bind(&q.underlying)
        .bind(q.expiry_days)
        .fetch_all(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, underlying = %q.underlying, "failed to load active option series");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|(scaled, scale)| scaled as f64 / 10_f64.powi(scale.clamp(0, 12) as i32))
        .collect::<Vec<_>>()
    } else {
        // Legacy simulated chains remain available until a series registry
        // exists for the requested underlying and expiry.
        (-7..=7)
            .map(|i| {
                (spot * (1.0 + i as f64 * 0.05) * 10000.0).round() / 10000.0
            })
            .filter(|strike| *strike > 0.0)
            .collect()
    };
    let mut chain = Vec::new();

    for strike in strikes {
        if strike <= 0.0 {
            continue;
        }

        let vol = smile_vol(base_vol, strike / spot);
        let call_inputs = BSInputs {
            spot,
            strike,
            vol,
            t,
            r,
            is_call: true,
        };
        let put_inputs = BSInputs {
            spot,
            strike,
            vol,
            t,
            r,
            is_call: false,
        };

        let call = black_scholes(&call_inputs);
        let put = black_scholes(&put_inputs);

        chain.push(OptionChainEntry {
            strike,
            expiry_days: q.expiry_days,
            is_itm_call: spot > strike,
            is_itm_put: spot < strike,
            call,
            put,
        });
    }

    Ok(Json(chain))
}

async fn get_implied_vol(
    State(state): State<AppState>,
    AppQuery(q): AppQuery<IvQuery>,
) -> Result<Json<IvResult>, StatusCode> {
    let prices = state.spot_prices.lock().unwrap();
    let spot = *prices.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;

    let t = q.expiry_days / 365.0;
    let is_call = q.option_type.to_lowercase() == "call";

    let iv = implied_vol(q.market_price, spot, q.strike, t, 0.05, is_call)
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;

    Ok(Json(IvResult { implied_vol: iv }))
}

async fn get_expiry_calendar(
    State(state): State<AppState>,
    Path(underlying): Path<String>,
) -> Result<Json<ExpiryCalendar>, StatusCode> {
    let prices = state.spot_prices.lock().unwrap();
    let vols = state.vol_surface.lock().unwrap();

    let spot = *prices.get(&underlying).ok_or(StatusCode::NOT_FOUND)?;
    let vol = *vols.get(&underlying).ok_or(StatusCode::NOT_FOUND)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let expiries = [7u32, 14, 21, 30, 60, 90, 180]
        .iter()
        .map(|&days| ExpiryInfo {
            days_to_expiry: days,
            label: format!("{days}D"),
            timestamp: now + days as u64 * 86400,
        })
        .collect();

    Ok(Json(ExpiryCalendar {
        underlying,
        spot,
        vol,
        expiries,
    }))
}

async fn get_protocol_stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let prices = state.spot_prices.lock().unwrap();
    Json(serde_json::json!({
        "total_series": 28,
        "active_series": 16,
        "total_premium_volume": 284_700.0,
        "open_interest": 1_420_000.0,
        "unique_traders": 312,
        "markets": prices.keys().collect::<Vec<_>>(),
        "network": "stellar-testnet"
    }))
}

// ─── App wiring ───────────────────────────────────────────────────────────────

/// Installs the global tracing subscriber. Must run before anything logs —
/// separated from `init_state`/`build_router` so tests can skip it (a test
/// binary installing a global subscriber per-test would panic on the
/// second one).
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zenith_backend=info,tower_http=debug".into()),
        )
        .init();
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Opens the database (from DATABASE_URL / .env, default sqlite://zenith.db)
/// and spawns the background loops (auth cleanup, alert checks, price
/// simulator) against it.
pub async fn init_state() -> AppState {
    dotenvy::dotenv().ok();
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://zenith.db".to_string());
    let pool = db::init_pool(&database_url).await;

    let state = AppState::new(pool);
    tokio::spawn(auth::cleanup_expired_loop(state.db.clone()));
    tokio::spawn(alerts::check_alerts_loop(state.clone()));
    tokio::spawn(prices::price_simulator_loop(state.clone()));
    state
}

/// Builds the full route table over a given AppState. Split out from
/// `init_state` so tests can build a router over a throwaway in-memory
/// database without spawning the background loops or touching .env.
/// The only two truly public, unauthenticated write endpoints — no login
/// required to hit them at all, so unlike everything else they need a
/// rate limit independent of any wallet's session. SmartIpKeyExtractor
/// gives each client its own quota (x-forwarded-for/x-real-ip/forwarded
/// header first, falling back to the TCP peer address via ConnectInfo —
/// wired up in main.rs's axum::serve call) instead of one shared quota
/// that a single abusive client could exhaust for everyone. Trusting
/// those headers assumes a reverse proxy that sets them correctly and
/// strips any client-supplied ones sits in front of this in production;
/// with no proxy, they simply won't be present and the peer-IP fallback
/// takes over.
fn auth_rate_limited_routes() -> Router<AppState> {
    use tower_governor::{
        governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor, GovernorLayer,
    };

    let config = Arc::new(
        GovernorConfigBuilder::default()
            .key_extractor(SmartIpKeyExtractor)
            .per_second(2)
            .burst_size(10)
            .finish()
            .expect("rate limiter config"),
    );

    Router::new()
        .route("/api/v1/auth/nonce", post(auth::post_nonce))
        .route("/api/v1/auth/verify", post(auth::post_verify))
        .layer(GovernorLayer { config })
}

/// Every write endpoint that mutates trading/account state, behind a more
/// generous quota than the auth endpoints (these require a valid session,
/// so abuse here is bounded by needing wallets in the first place — but a
/// single compromised or careless client still shouldn't be able to
/// hammer the DB with unlimited opens/closes/rolls). Keyed per-wallet
/// (via BearerOrIpKeyExtractor) rather than per-IP like the auth routes:
/// every route here already requires a session token, and keying on IP
/// alone would mean wallets sharing a NAT/VPN/corporate network split one
/// quota instead of each getting their own.
fn mutation_rate_limited_routes() -> Router<AppState> {
    use crate::rate_limit_key::BearerOrIpKeyExtractor;
    use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

    let config = Arc::new(
        GovernorConfigBuilder::default()
            .key_extractor(BearerOrIpKeyExtractor)
            .per_second(5)
            .burst_size(20)
            .finish()
            .expect("rate limiter config"),
    );

    Router::new()
        .route("/api/v1/positions/open", post(positions::open_position))
        .route(
            "/api/v1/positions/:id/close",
            post(positions::close_position),
        )
        .route(
            "/api/v1/positions/:id/exercise",
            post(partial_exercise::exercise_position),
        )
        .route("/api/v1/positions/:id/roll", post(positions::roll_position))
        .route("/api/v1/watchlist", post(watchlist::add_watchlist))
        .route("/api/v1/alerts", post(alerts::create_alert))
        .route(
            "/api/v1/strategies/execute",
            post(strategies::execute_strategy),
        )
        .route(
            "/api/v1/strategies/:id/close",
            post(strategies::close_strategy),
        )
        .route("/api/v1/tx/prepare", post(chain::handlers::post_prepare))
        .route("/api/v1/tx/submit", post(chain::handlers::post_submit))
        .layer(GovernorLayer { config })
}

fn request_id_header() -> axum::http::HeaderName {
    axum::http::HeaderName::from_static("x-request-id")
}

pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    Router::new()
        .route("/health", get(health))
        .route("/admin/settlement", get(operator::settlement_dashboard))
        .route("/api/v1/spot", get(get_spot))
        .route("/api/v1/price", get(price_option))
        .route("/api/v1/iv", get(get_implied_vol))
        .route("/api/v1/chain", get(get_chain))
        .route(
            "/api/v1/settlement/preview",
            post(preview::preview_settlement),
        )
        .route("/api/v1/series", get(series_lifecycle::list_active_series))
        .route(
            "/api/v1/series/:id/halt",
            post(series_lifecycle::halt_series),
        )
        .route(
            "/api/v1/series/:id/resume",
            post(series_lifecycle::resume_series),
        )
        .route(
            "/api/v1/series/:id/delist",
            post(series_lifecycle::delist_series),
        )
        .route(
            "/api/v1/series/:id/settle",
            post(series_lifecycle::settle_series_early),
        )
        .route(
            "/api/v1/series/:id/payoff",
            post(payoff::set_series_payoff),
        )
        .route(
            "/api/v1/payoff/registry/:kind",
            post(payoff::configure_payoff),
        )
        .route("/api/v1/expiries/:underlying", get(get_expiry_calendar))
        .route("/api/v1/stats", get(get_protocol_stats))
        .route("/api/v2/spot", get(api_version::get_spot_v2))
        .route("/api/versions/usage", get(api_version::usage))
        .merge(auth_rate_limited_routes())
        .merge(mutation_rate_limited_routes())
        .route("/api/v1/auth/me", get(auth::get_me))
        .route("/api/v1/account", get(positions::get_account))
        .route("/api/v1/positions", get(positions::list_positions))
        .route("/api/v1/history", get(history::get_history))
        .route("/api/v1/watchlist", get(watchlist::get_watchlist))
        .route(
            "/api/v1/watchlist/:underlying",
            axum::routing::delete(watchlist::remove_watchlist),
        )
        .route("/api/v1/alerts", get(alerts::get_alerts))
        .route(
            "/api/v1/alerts/:id",
            axum::routing::delete(alerts::delete_alert),
        )
        .route("/api/v1/strategies", get(strategies::list_strategies))
        .route("/api/v1/strategies/:id", get(strategies::get_strategy))
        .route("/api/v1/ws/spot", get(prices::ws_spot))
        .route("/api/v1/portfolio/payoff", post(payoff::post_portfolio_payoff))
        .route("/api/v1/payoff/preview", post(payoff::post_payoff))
        .route(
            "/api/v1/portfolio/greeks",
            get(positions::get_portfolio_greeks),
        )
        .route("/api/v1/risk/stress", post(risk_stress::stress_portfolios))
        // ── Merkle-commitment batch settlement (#196) ──────────────────────
        .route(
            "/api/v1/settlement/batches",
            post(chain::settlement::plan_batch),
        )
        .route(
            "/api/v1/settlement/batches/:id",
            get(chain::settlement::get_batch),
        )
        .route(
            "/api/v1/settlement/batches/:id/commit",
            post(chain::settlement::commit_batch),
        )
        .route(
            "/api/v1/settlement/batches/:id/leaves",
            get(chain::settlement::list_leaves),
        )
        .route(
            "/api/v1/settlement/batches/:id/leaves/:position_id/proof",
            get(chain::settlement::get_proof),
        )
        .route(
            "/api/v1/settlement/batches/:id/claims",
            post(chain::settlement::claim),
        )
        .route("/api/v1/tx/:handle", get(chain::handlers::get_preparation))
        // Failed-settlement diagnostics for operators (Bearer ZENITH_ADMIN_TOKEN).
        .nest_service(
            "/api/v1/admin/tx-forensics",
            soroban::forensics::router(state.db.clone(), std::env::var("ZENITH_ADMIN_TOKEN").ok()),
        .route("/api/v1/keeper/bounties", get(keeper::bounty::accounting))
        .route("/api/v1/privacy/export", get(privacy::export))
        .route(
            "/api/v1/privacy/erasure",
            axum::routing::delete(privacy::erase),
        )
        .route(
            "/api/v1/settlement/statements",
            get(settlement_reports::statement),
        )
        .layer(PropagateRequestIdLayer::new(request_id_header()))
        .layer(axum::middleware::from_fn(
            api_version::negotiate_version,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::new(
            request_id_header(),
            request_id::MakeRequestUuid,
        ))
        .layer(cors)
        .with_state(state)
}

/// Waits for Ctrl+C or SIGTERM so axum stops accepting new connections
/// and finishes in-flight requests instead of dropping them mid-response
/// on a container stop/redeploy.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received, draining in-flight requests");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smile_vol_at_the_money_equals_base() {
        // At m=0 the skew/curvature terms vanish but the wing term doesn't
        // (0.15^2 unconditionally), matching the frontend's shipped behavior.
        let base = 0.82;
        let v = smile_vol(base, 1.0);
        assert!((v - (base + 0.12 * 0.15_f64.powi(2))).abs() < 1e-12);
    }

    #[test]
    fn implied_vol_round_trips_through_black_scholes() {
        let spot = 100.0;
        let strike = 105.0;
        let t = 30.0 / 365.0;
        let r = 0.05;
        let true_vol = 0.65;

        let price = black_scholes(&BSInputs {
            spot,
            strike,
            vol: true_vol,
            t,
            r,
            is_call: true,
        })
        .premium;
        let recovered = implied_vol(price, spot, strike, t, r, true).unwrap();

        assert!((recovered - true_vol).abs() < 1e-4);
    }

    #[test]
    fn implied_vol_rejects_price_below_intrinsic() {
        let spot = 100.0;
        let strike = 80.0;
        // Call intrinsic is 20; a market price below that is arbitrage-free-impossible.
        assert!(implied_vol(10.0, spot, strike, 30.0 / 365.0, 0.05, true).is_none());
    }
}
