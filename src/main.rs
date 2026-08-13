use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::{f64::consts::PI, sync::Arc};
use tower_http::cors::{Any, CorsLayer};
use axum::http::Method;

// ─── Black-Scholes Pricing Engine ─────────────────────────────────────────────

/// Cumulative standard normal distribution (Abramowitz & Stegun approximation)
fn norm_cdf(x: f64) -> f64 {
    if x < -7.0 { return 0.0; }
    if x >  7.0 { return 1.0; }
    let k = 1.0 / (1.0 + 0.2316419 * x.abs());
    let poly = k * (0.319381530
        + k * (-0.356563782
        + k * (1.781477937
        + k * (-1.821255978
        + k * 1.330274429))));
    let pdf = (-x * x / 2.0).exp() / (2.0 * PI).sqrt();
    if x >= 0.0 { 1.0 - pdf * poly } else { pdf * poly }
}

/// Standard normal PDF
fn norm_pdf(x: f64) -> f64 {
    (-x * x / 2.0).exp() / (2.0 * PI).sqrt()
}

#[derive(Debug, Clone)]
pub struct BSInputs {
    pub spot: f64,       // current price
    pub strike: f64,     // strike price
    pub vol: f64,        // annualised volatility (e.g. 0.80 = 80%)
    pub t: f64,          // time to expiry in years
    pub r: f64,          // risk-free rate (e.g. 0.05 = 5%)
    pub is_call: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BSResult {
    pub premium: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,      // per day
    pub vega: f64,       // per 1% vol move
    pub rho: f64,
    pub d1: f64,
    pub d2: f64,
    pub intrinsic: f64,
    pub time_value: f64,
    pub iv: f64,
}

pub fn black_scholes(inputs: &BSInputs) -> BSResult {
    let BSInputs { spot: s, strike: k, vol: sigma, t, r, is_call } = *inputs;

    if t <= 0.0 {
        // At expiry: intrinsic only
        let intrinsic = if is_call {
            (s - k).max(0.0)
        } else {
            (k - s).max(0.0)
        };
        return BSResult {
            premium: intrinsic, delta: if is_call { 1.0 } else { -1.0 },
            gamma: 0.0, theta: 0.0, vega: 0.0, rho: 0.0,
            d1: 0.0, d2: 0.0, intrinsic, time_value: 0.0, iv: sigma,
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
        let del   = nd1;
        let r_val = k * t * disc * nd2 / 100.0;
        (price, del, r_val)
    } else {
        let nd1 = norm_cdf(-d1);
        let nd2 = norm_cdf(-d2);
        let price = k * disc * nd2 - s * nd1;
        let del   = nd1 - 1.0;
        let r_val = -k * t * disc * nd2 / 100.0;
        (price, del, r_val)
    };

    let pdf_d1 = norm_pdf(d1);
    let gamma  = pdf_d1 / (s * sigma * sqrt_t);
    let vega   = s * pdf_d1 * sqrt_t / 100.0;  // per 1% vol

    let theta = if is_call {
        (-(s * pdf_d1 * sigma) / (2.0 * sqrt_t)
            - r * k * disc * norm_cdf(d2)) / 365.0
    } else {
        (-(s * pdf_d1 * sigma) / (2.0 * sqrt_t)
            + r * k * disc * norm_cdf(-d2)) / 365.0
    };

    let intrinsic = if is_call { (s - k).max(0.0) } else { (k - s).max(0.0) };
    let time_value = premium - intrinsic;

    BSResult { premium, delta, gamma, theta, vega, rho, d1, d2, intrinsic, time_value, iv: sigma }
}

/// Newton-Raphson implied volatility solver
pub fn implied_vol(market_price: f64, spot: f64, strike: f64, t: f64, r: f64, is_call: bool) -> Option<f64> {
    let intrinsic = if is_call { (spot - strike).max(0.0) } else { (strike - spot).max(0.0) };
    if market_price < intrinsic { return None; }

    let mut sigma = 0.5_f64; // initial guess
    for _ in 0..100 {
        let bs = black_scholes(&BSInputs { spot, strike, vol: sigma, t, r, is_call });
        let diff = bs.premium - market_price;
        if diff.abs() < 1e-6 { return Some(sigma); }
        if bs.vega.abs() < 1e-10 { break; }
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
}

impl AppState {
    pub fn new() -> Self {
        let mut prices = std::collections::HashMap::new();
        prices.insert("XLM".into(), 0.1182);
        prices.insert("BTC".into(), 67420.50);
        prices.insert("ETH".into(), 3512.80);
        prices.insert("SOL".into(), 182.45);

        let mut vols = std::collections::HashMap::new();
        vols.insert("XLM".into(), 0.82);  // 82% ann vol
        vols.insert("BTC".into(), 0.65);
        vols.insert("ETH".into(), 0.72);
        vols.insert("SOL".into(), 0.91);

        Self {
            spot_prices: Arc::new(std::sync::Mutex::new(prices)),
            vol_surface: Arc::new(std::sync::Mutex::new(vols)),
        }
    }
}

// ─── Request / Response Types ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PriceQuery {
    pub underlying: String,
    pub strike: f64,
    pub expiry_days: f64,
    pub option_type: String,   // "call" | "put"
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

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "zenith-backend",
        "version": "0.1.0",
        "network": "stellar-testnet"
    }))
}

async fn get_spot(State(state): State<AppState>) -> Json<SpotResponse> {
    let prices = state.spot_prices.lock().unwrap().clone();
    let vols   = state.vol_surface.lock().unwrap().clone();
    Json(SpotResponse { prices, vols })
}

async fn price_option(
    State(state): State<AppState>,
    Query(q): Query<PriceQuery>,
) -> Result<Json<BSResult>, StatusCode> {
    let prices = state.spot_prices.lock().unwrap();
    let vols   = state.vol_surface.lock().unwrap();

    let spot = *prices.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;
    let vol  = *vols.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;

    let t = q.expiry_days / 365.0;
    let is_call = q.option_type.to_lowercase() == "call";

    let result = black_scholes(&BSInputs { spot, strike: q.strike, vol, t, r: 0.05, is_call });
    Ok(Json(result))
}

async fn get_chain(
    State(state): State<AppState>,
    Query(q): Query<ChainQuery>,
) -> Result<Json<Vec<OptionChainEntry>>, StatusCode> {
    let prices = state.spot_prices.lock().unwrap();
    let vols   = state.vol_surface.lock().unwrap();

    let spot = *prices.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;
    let vol  = *vols.get(&q.underlying).ok_or(StatusCode::NOT_FOUND)?;
    let t    = q.expiry_days / 365.0;
    let r    = 0.05_f64;

    // Generate strikes: ±30% from spot in 5% increments
    let step_count = 7;
    let step_pct   = 0.05_f64;
    let mut chain  = Vec::new();

    for i in -(step_count as i32)..=(step_count as i32) {
        let strike = (spot * (1.0 + i as f64 * step_pct) * 100.0).round() / 100.0;
        if strike <= 0.0 { continue; }

        let call_inputs = BSInputs { spot, strike, vol, t, r, is_call: true };
        let put_inputs  = BSInputs { spot, strike, vol, t, r, is_call: false };

        let call = black_scholes(&call_inputs);
        let put  = black_scholes(&put_inputs);

        chain.push(OptionChainEntry {
            strike,
            expiry_days: q.expiry_days,
            is_itm_call: spot > strike,
            is_itm_put:  spot < strike,
            call,
            put,
        });
    }

    Ok(Json(chain))
}

async fn get_expiry_calendar(
    State(state): State<AppState>,
    Path(underlying): Path<String>,
) -> Result<Json<ExpiryCalendar>, StatusCode> {
    let prices = state.spot_prices.lock().unwrap();
    let vols   = state.vol_surface.lock().unwrap();

    let spot = *prices.get(&underlying).ok_or(StatusCode::NOT_FOUND)?;
    let vol  = *vols.get(&underlying).ok_or(StatusCode::NOT_FOUND)?;

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

    Ok(Json(ExpiryCalendar { underlying, spot, vol, expiries }))
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

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let state = AppState::new();

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/spot", get(get_spot))
        .route("/api/v1/price", get(price_option))
        .route("/api/v1/chain", get(get_chain))
        .route("/api/v1/expiries/:underlying", get(get_expiry_calendar))
        .route("/api/v1/stats", get(get_protocol_stats))
        .layer(cors)
        .with_state(state);

    let addr = "0.0.0.0:8081";
    println!("Zenith backend listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
