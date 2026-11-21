//! Criterion benchmark for the implied-volatility solver.
//!
//! Run with:
//! ```text
//! cargo bench --bench iv
//! ```
//!
//! Each case prints a line like `implied_vol/<label>  time: [lo mid hi]`; the
//! `mid` estimate is the per-call solver cost (setup is excluded from the timed
//! section), and the bracketed range shows run-to-run variance.
//!
//! The benchmark exercises the safeguarded Newton/Brent solver across a grid
//! of moneyness, expiry and volatility values that cover the acceptance
//! criteria of issue #21 (vol in [0.01, 10], T in [1 hour, 3 years],
//! moneyness in [0.2, 5]).

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
        // Setup: build the input vector once, outside the timed closure, so the
        // measured section reflects solver cost only (no per-iteration alloc).
        let price = black_scholes(
            case.option_type,
            case.spot,
            case.strike,
            case.rate,
            case.dividend,
            case.expiry,
            case.vol,
        );

        let inputs = [
            case.option_type,
            case.spot,
            case.strike,
            case.rate,
            case.dividend,
            case.expiry,
            price,
        ];

        group.bench_with_input(BenchmarkId::from_parameter(case.label), &inputs, |b, inputs| {
            b.iter(|| {
                let iv = implied_vol(
                    black_box(inputs[0]),
                    black_box(inputs[1]),
                    black_box(inputs[2]),
                    black_box(inputs[3]),
                    black_box(inputs[4]),
                    black_box(inputs[5]),
                    black_box(inputs[6]),
                );
                black_box(iv)
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_implied_vol);
criterion_main!(benches);
