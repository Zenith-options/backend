//! Criterion benchmark for the implied-volatility solver.
//!
//! Run with:
//! ```text
//! cargo bench --bench iv
//! ```
//!
//! To run a specific case only:
//! ```text
//! cargo bench --bench iv -- implied_vol/atm-1y-20vol
//! ```
//!
//! To output results in a machine-readable format (JSON):
//! ```text
//! cargo bench --bench iv -- --output-format json
//! ```
//!
//! The benchmark exercises the safeguarded Newton/Brent solver across a grid
//! of moneyness, expiry and volatility values that cover the acceptance
//! criteria of issue #21 (vol in [0.01, 10], T in [1 hour, 3 years],
//! moneyness in [0.2, 5]).
//!
//! **Reading the output**
//! - Criterion reports time per iteration in nanoseconds (ns/iter).
//! - Lower is better. Typical values for this solver: 1,000–10,000 ns/iter depending on case difficulty.
//! - The `Throughput` column shows elements per second (1 / time_per_iter).
//! - Each case label encodes: moneyness (spot/strike ratio), expiry in years, input volatility.
//!   Examples: `atm-1y-20vol` = at-the-money, 1 year expiry, 20% vol.
//! - The grid covers:
//!   - ATM (moneyness 1.0) at 1h, 1y, 3y with 20% vol
//!   - Deep OTM (0.2) at 1y with 20% vol, 1h with 80% vol
//!   - Deep ITM (5.0) at 1y with 20% vol, 3y with 1% vol
//!   - OTM (0.5) at 1h with 1000% vol (extreme edge case)
//! - Acceptance criteria (issue #21): solver must converge within 50 iterations
//!   and return a vol within 1% relative error of the input vol across the grid.
//!
//! **Interpreting performance changes**
//! - A regression > 20% in ns/iter warrants investigation (algorithm change, not noise).
//! - Compare `cargo bench --bench iv` before and after changes using `cargo bench --bench iv -- --save-baseline main` and `cargo bench --bench iv -- --baseline main`.
//!
//! **Dependencies**: requires the `option-pricing` crate (internal) with the `implied_vol` function.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use option_pricing::{black_scholes, implied_vol, OptionType};

/// One representative point on the (moneyness, expiry, vol) grid.
struct Case {
    label: &'static str,
    spot: f64,
    strike: f64,
    rate: f64,
    dividend: f64,
    expiry: f64,
    vol: f64,
    option_type: OptionType,
}

fn cases() -> Vec<Case> {
    let spot = 100.0;
    let rate = 0.02;
    let dividend = 0.0;

    // (moneyness, expiry, vol, label)
    let grid: &[(f64, f64, f64, &'static str)] = &[
        (1.0, 1.0, 0.20, "atm-1y-20vol"),
        (1.0, 1.0 / 8760.0, 0.20, "atm-1h-20vol"),
        (1.0, 3.0, 0.20, "atm-3y-20vol"),
        (0.2, 1.0, 0.20, "deep-otm-1y-20vol"),
        (5.0, 1.0, 0.20, "deep-itm-1y-20vol"),
        (0.2, 1.0 / 8760.0, 0.80, "deep-otm-1h-80vol"),
        (5.0, 3.0, 0.01, "deep-itm-3y-1vol"),
        (0.5, 1.0 / 8760.0, 10.0, "otm-1h-1000vol"),
    ];

    grid.iter()
        .map(|&(moneyness, expiry, vol, label)| Case {
            label,
            spot,
            strike: spot / moneyness,
            rate,
            dividend,
            expiry,
            vol,
            option_type: OptionType::Call,
        })
        .collect()
}

fn bench_implied_vol(c: &mut Criterion) {
    let cases = cases();

    let mut group = c.benchmark_group("implied_vol");
    group.throughput(Throughput::Elements(1));

    for case in &cases {
        let price = black_scholes(
            case.option_type,
            case.spot,
            case.strike,
            case.rate,
            case.dividend,
            case.expiry,
            case.vol,
        );

        group.bench_with_input(BenchmarkId::from_parameter(case.label), &price, |b, &price| {
            b.iter(|| {
                let iv = implied_vol(
                    black_box(case.option_type),
                    black_box(case.spot),
                    black_box(case.strike),
                    black_box(case.rate),
                    black_box(case.dividend),
                    black_box(case.expiry),
                    black_box(price),
                );
                black_box(iv)
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_implied_vol);
criterion_main!(benches);
