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

pub mod alerts;
pub mod auth;
pub mod collateral;
pub mod db;
pub mod error;
pub mod history;
pub mod models;
pub mod payoff;
pub mod positions;
pub mod prices;
pub mod rate_limit_key;
pub mod request_id;
pub mod strategies;
pub mod strkey;
pub mod watchlist;

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
pub(crate) fn smile_vol(base: f64, moneyness: f64) -> f64 {
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

// ─── Implied Volatility Solver ────────────────────────────────────────────────

/// Typed errors returned by [`implied_vol`].
///
/// Each variant maps to a distinct HTTP 422 message at the API boundary so
/// callers can tell apart "the market price is impossible" from "the solver
/// failed to converge".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IvError {
    /// Market price is at or below the option's intrinsic value.
    BelowIntrinsic,
    /// Market price exceeds the no-arbitrage upper bound for the option.
    AboveUpperBound,
    /// The solver exhausted its iteration budget without reaching tolerance.
    NoConvergence,
    /// Inputs were non-finite, non-positive, or otherwise unusable.
    InvalidInput,
}

impl IvError {
    /// Human-readable message used when mapping to HTTP 422 responses.
    pub fn message(&self) -> &'static str {
        match self {
            IvError::BelowIntrinsic => {
                "market price is at or below the option's intrinsic value"
            }
            IvError::AboveUpperBound => {
                "market price exceeds the no-arbitrage upper bound for this option"
            }
            IvError::NoConvergence => {
                "implied volatility solver failed to converge for the given inputs"
            }
            IvError::InvalidInput => {
                "invalid inputs: spot, strike, time, and price must be finite and positive"
            }
        }
    }
}

impl std::fmt::Display for IvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for IvError {}

/// Lower bound on the volatility search bracket.
const IV_MIN_VOL: f64 = 0.01;
/// Upper bound on the volatility search bracket.
const IV_MAX_VOL: f64 = 10.0;
/// Target absolute price error for convergence.
const IV_TOL: f64 = 1e-10;
/// Maximum safeguarded-Newton / Brent iterations.
const IV_MAX_ITER: usize = 200;

/// No-arbitrage upper bound for a European option.
fn iv_upper_bound(spot: f64, strike: f64, t: f64, r: f64, is_call: bool) -> f64 {
    let disc = (-r * t).exp();
    if is_call {
        spot
    } else {
        strike * disc
    }
}

/// Brenner–Subrahmanyam style initial guess for the implied volatility.
///
/// Uses the at-the-money approximation `sigma ~ sqrt(2*pi/T) * price / spot`
/// which is accurate near the money and a reasonable starting point for the
/// safeguarded Newton iteration elsewhere.
fn iv_initial_guess(market_price: f64, spot: f64, strike: f64, t: f64, r: f64) -> f64 {
    let disc = (-r * t).exp();
    let fwd = spot / disc;
    let atm = 0.5 * (spot + strike * disc);
    let _ = fwd;
    let denom = atm.max(1e-12);
    let guess = (2.0 * PI / t).sqrt() * market_price / denom;
    guess.clamp(IV_MIN_VOL, IV_MAX_VOL)
}

/// Robust implied-volatility solver.
///
/// Combines a safeguarded Newton–Raphson step with a bracketed fallback
/// (bisection / Brent-style) so that deep-OTM options, very short expiries,
/// and high-vol regimes converge instead of diverging or oscillating.
///
/// Returns `Ok(sigma)` with `|price(sigma) - market_price| < 1e-10` when the
/// market price lies strictly inside the no-arbitrage bounds, or a typed
/// [`IvError`] otherwise.
pub fn implied_vol(
    market_price: f64,
    spot: f64,
    strike: f64,
    t: f64,
    r: f64,
    is_call: bool,
) -> Result<f64, IvError> {
    // ── Input validation ──────────────────────────────────────────────────
    if !market_price.is_finite()
        || !spot.is_finite()
        || !strike.is_finite()
        || !t.is_finite()
        || !r.is_finite()
        || spot <= 0.0
        || strike <= 0.0
        || t <= 0.0
        || market_price <= 0.0
    {
        return Err(IvError::InvalidInput);
    }

    let intrinsic = if is_call {
        (spot - strike).max(0.0)
    } else {
        (strike - spot).max(0.0)
    };
    let upper = iv_upper_bound(spot, strike, t, r, is_call);

    // Price exactly at intrinsic corresponds to the IV = 0 limit; the issue
    // asks us to reject prices at or below intrinsic with a typed error.
    if market_price <= intrinsic {
        return Err(IvError::BelowIntrinsic);
    }
    if market_price >= upper {
        return Err(IvError::AboveUpperBound);
    }

    // ── Bracket setup ─────────────────────────────────────────────────────
    let price_at = |sigma: f64| -> f64 {
        black_scholes(&BSInputs {
            spot,
            strike,
            vol: sigma,
            t,
            r,
            is_call,
        })
        .premium
    };

    let mut lo = IV_MIN_VOL;
    let mut hi = IV_MAX_VOL;
    let mut f_lo = price_at(lo) - market_price;
    let mut f_hi = price_at(hi) - market_price;

    // If the bracket does not straddle the target, the price is outside the
    // reachable range for vol in [IV_MIN_VOL, IV_MAX_VOL].
    if f_lo > 0.0 {
        return Err(IvError::BelowIntrinsic);
    }
    if f_hi < 0.0 {
        return Err(IvError::AboveUpperBound);
    }

    // ── Safeguarded Newton with bracketed fallback ────────────────────────
    let mut sigma = iv_initial_guess(market_price, spot, strike, t, r);
    if sigma <= lo || sigma >= hi {
        sigma = 0.5 * (lo + hi);
    }

    let mut last_bisect = false;
    for _ in 0..IV_MAX_ITER {
        let bs = black_scholes(&BSInputs {
            spot,
            strike,
            vol: sigma,
            t,
            r,
            is_call,
        });
        let diff = bs.premium - market_price;
        if diff.abs() < IV_TOL {
            return Ok(sigma);
        }

        // Maintain the bracket using the sign of the residual.
        if diff > 0.0 {
            hi = sigma;
            f_hi = diff;
        } else {
            lo = sigma;
            f_lo = diff;
        }

        // Newton step (vega is per 1% vol, so scale by 100 for per-unit).
        let vega_unit = bs.vega * 100.0;
        let mut next = if vega_unit.abs() > 1e-14 {
            sigma - diff / vega_unit
        } else {
            f64::NAN
        };

        // Fall back to bisection when the Newton step is unusable, leaves the
        // bracket, or fails to reduce the residual (oscillation guard).
        let newton_ok = next.is_finite()
            && next > lo
            && next < hi
            && (next - sigma).abs() < 0.5 * (hi - lo);

        if !newton_ok {
            next = 0.5 * (lo + hi);
            last_bisect = true;
        } else {
            last_bisect = false;
        }

        // Brent-style secant refinement when we have a valid bracket and the
        // last step was not a forced bisection.
        if !last_bisect && f_hi != f_lo {
            let secant = hi - f_hi * (hi - lo) / (f_hi - f_lo);
            if secant.is_finite() && secant > lo && secant < hi {
                next = secant;
            }
        }

        if (next - sigma).abs() < 1e-15 {
            // No further progress possible; accept if within tolerance.
            let final_diff = price_at(next) - market_price;
            if final_diff.abs() < IV_TOL {
                return Ok(next);
            }
            return Err(IvError::NoConvergence);
        }
        sigma = next;
    }

    // Final tolerance check after the iteration budget is exhausted.
    if (price_at(sigma) - market_price).abs() < IV_TOL {
        Ok(sigma)
    } else {
        Err(IvError::NoConvergence)
    }
}

// ─── Market Data ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub spot_prices: Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
    pub vol_surface: Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
    pub db: sqlx::SqlitePool,
    /// Broadcasts a JSON-encoded SpotResponse every time the price
    /// simulator nudges spot_prices, for the /api/v1/ws/spot handler to
    /// forward to connected clients. `send` errors (no receivers) are
    /// expected and ignored — the simulator runs regardless of whether
    /// anyone's listening.
    pub spot_tx: tokio::sync::broadcast::Sender<String>,
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

        Self {
            spot_prices: Arc::new(std::sync::Mutex::new(prices)),
            vol_surface: Arc::new(std::sync::Mutex::new(vols)),
            db,
            spot_tx,
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

/* … truncated 14639 chars — edit only what you need near the top … */
