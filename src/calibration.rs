//! Calibration of `compare` on synthetic runs (see `synth`): how often it
//! names a direction when A and B are the same distribution (false-verdict
//! rate), and how often it says Slower when B really is slower (power).
//!
//! Report only, no assertions yet. Slow, so ignored; run with
//! `cargo test --release --lib calibration -- --ignored --nocapture`.
//! It prints Markdown tables, for the README.

use std::time::Duration;

use crate::compare::{Comparison, DEFAULT_MIN_EFFECT, DEFAULT_SEED, RecordedRun, Verdict, compare};
use crate::manifest::Manifest;
use crate::stats::{self, DEFAULT_LEVEL, DEFAULT_RESAMPLES};
use crate::synth::Synth;

const SIGMA: f64 = 0.5;
const PHI: f64 = 0.5;
/// Between-run sd of each setting, as a fraction of the 10 ms median.
const RUN_SD_FRACTIONS: [f64; 2] = [0.0, 0.03];

const FALSE_RUNS: [usize; 3] = [3, 5, 10];
const FALSE_TRIALS: u64 = 500;
const FALSE_QUANTILES: [f64; 2] = [0.5, 0.99];

const POWER_RUNS: usize = 5;
const POWER_TRIALS: u64 = 200;
const POWER_SCALES: [f64; 5] = [1.00, 1.02, 1.05, 1.10, 1.20];
const POWER_Q: f64 = 0.99;

/// Runs for estimating the true p99 of the model: 1000 x 1000 samples.
const TRUTH_RUNS: u64 = 1000;

/// What one comparison said, at min_effect 0 and at `DEFAULT_MIN_EFFECT`.
#[derive(Debug, Clone, Copy, Default)]
struct Outcome {
    slower: bool,
    faster: bool,
    /// Slower or Faster AND |delta| >= DEFAULT_MIN_EFFECT.
    directional_at_default: bool,
}

/// `compare` is run once, with min_effect 0. The min_effect rule only ever
/// ADDS an inconclusive reason, so the verdict at `DEFAULT_MIN_EFFECT` is
/// directional exactly when this one is and |delta| >= DEFAULT_MIN_EFFECT.
fn outcome(c: &Comparison) -> Outcome {
    let min_ns = DEFAULT_MIN_EFFECT.as_nanos();
    match c.verdict {
        Verdict::Slower { delta, .. } => Outcome {
            slower: true,
            directional_at_default: u128::from(delta.unsigned_abs()) >= min_ns,
            ..Outcome::default()
        },
        Verdict::Faster { delta, .. } => Outcome {
            faster: true,
            directional_at_default: u128::from(delta.unsigned_abs()) >= min_ns,
            ..Outcome::default()
        },
        Verdict::Inconclusive { .. } => Outcome::default(),
        Verdict::Refused { .. } => panic!("synthetic runs are comparable: {c}"),
    }
}

/// `f(0..n)` on all cores, in order.
fn par_map<T: Send>(n: u64, f: impl Fn(u64) -> T + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get() as u64);
    let mut out: Vec<(u64, T)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let f = &f;
                scope.spawn(move || (k..n).step_by(threads as usize).map(|i| (i, f(i))).collect::<Vec<_>>())
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("worker panicked"))
            .collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, t)| t).collect()
}

/// Seed of run `run` of side `side` (0 = A, 1 = B) in trial `trial` of
/// table cell `cell`: distinct for every run of the report.
fn seed(cell: u64, trial: u64, side: u64, run: usize) -> u64 {
    (cell << 40) | (trial << 12) | (side << 8) | run as u64
}

fn runs(model: &Synth, m: &Manifest, cell: u64, trial: u64, side: u64, r: usize, scale: f64) -> Vec<RecordedRun> {
    (0..r)
        .map(|i| model.run(m, seed(cell, trial, side, i), scale))
        .collect()
}

/// "0.046 (23/500)".
fn rate(hits: usize, trials: u64) -> String {
    format!("{:.3} ({hits}/{trials})", hits as f64 / trials as f64)
}

fn count(outcomes: &[Outcome], f: impl Fn(&Outcome) -> bool) -> usize {
    outcomes.iter().filter(|o| f(o)).count()
}

fn report_setting(index: u64, run_sd_fraction: f64) {
    let model = Synth::new(SIGMA, PHI, run_sd_fraction * Synth::MEDIAN_NS);
    let m = model.manifest();
    let p = |q| stats::quantile_label(q);

    println!(
        "### Between-run sd = {:.0}% of median ({:.1} ms)\n",
        run_sd_fraction * 100.0,
        model.run_sd_ns / 1e6
    );
    println!(
        "Model: lognormal body, median {:.0} ms, sigma {SIGMA}; AR(1) phi {PHI}; {} samples per run; \
         nanosecond resolution.",
        model.median_ns / 1e6,
        model.n
    );
    println!(
        "compare: hierarchical bootstrap, {DEFAULT_RESAMPLES} resamples, nominal {}%, seed {DEFAULT_SEED:#018X}; \
         min_effect 0 and {} ms (the default).\n",
        DEFAULT_LEVEL * 100.0,
        DEFAULT_MIN_EFFECT.as_secs_f64() * 1e3
    );

    // False-verdict rate: A and B from the same model.
    println!(
        "False-verdict rate: A and B from the same distribution, {FALSE_TRIALS} trials per cell. A false verdict \
         is Slower or Faster. Target <= 0.05.\n"
    );
    let mut header = String::from("| R per side |");
    let mut rule = String::from("|---:|");
    for q in FALSE_QUANTILES {
        header += &format!(" {}, min_effect 0 | {}, min_effect 1 ms |", p(q), p(q));
        rule += "---:|---:|";
    }
    println!("{header}\n{rule}");
    for (k, r) in (0u64..).zip(FALSE_RUNS) {
        let cell = index * 100 + k;
        let per_trial: Vec<Vec<Outcome>> = par_map(FALSE_TRIALS, |t| {
            let a = runs(&model, &m, cell, t, 0, r, 1.0);
            let b = runs(&model, &m, cell, t, 1, r, 1.0);
            FALSE_QUANTILES
                .iter()
                .map(|&q| outcome(&compare(&a, &b, q, Duration::ZERO)))
                .collect()
        });
        let mut row = format!("| {r} |");
        for qi in 0..FALSE_QUANTILES.len() {
            let col: Vec<Outcome> = per_trial.iter().map(|o| o[qi]).collect();
            row += &format!(
                " {} | {} |",
                rate(count(&col, |o| o.slower || o.faster), FALSE_TRIALS),
                rate(count(&col, |o| o.directional_at_default), FALSE_TRIALS)
            );
        }
        println!("{row}");
    }

    // Power: B's latency scaled.
    let truth_samples: Vec<u64> = (0..TRUTH_RUNS)
        .flat_map(|run| model.samples(u64::MAX - run, 1.0))
        .map(|s| s.latency_ns)
        .collect();
    let truth = stats::percentile(&truth_samples, POWER_Q).expect("non-empty") as f64;
    println!(
        "\nPower at {}: B's latency multiplied by a scale, R = {POWER_RUNS} per side, {POWER_TRIALS} trials per \
         scale. True {} of A is {:.2} ms (from {} samples), so the true delta is (scale - 1) x that.\n",
        p(POWER_Q),
        p(POWER_Q),
        truth / 1e6,
        truth_samples.len()
    );
    println!("| B scale | true delta | Slower, min_effect 0 | Slower, min_effect 1 ms | Faster (wrong way) |");
    println!("|---:|---:|---:|---:|---:|");
    for (k, scale) in (0u64..).zip(POWER_SCALES) {
        let cell = index * 100 + 50 + k;
        let outcomes = par_map(POWER_TRIALS, |t| {
            let a = runs(&model, &m, cell, t, 0, POWER_RUNS, 1.0);
            let b = runs(&model, &m, cell, t, 1, POWER_RUNS, scale);
            outcome(&compare(&a, &b, POWER_Q, Duration::ZERO))
        });
        println!(
            "| {scale:.2} | {:.2} ms | {} | {} | {} |",
            (scale - 1.0) * truth / 1e6,
            rate(count(&outcomes, |o| o.slower), POWER_TRIALS),
            rate(count(&outcomes, |o| o.slower && o.directional_at_default), POWER_TRIALS),
            rate(count(&outcomes, |o| o.faster), POWER_TRIALS)
        );
    }
    println!();
}

#[test]
#[ignore = "slow: about 10 000 bootstrap comparisons; run with --release --ignored --nocapture"]
fn calibration_report() {
    let start = std::time::Instant::now();
    println!("## Calibration of compare\n");
    println!(
        "Monte Carlo error: at a true rate of 0.05, {FALSE_TRIALS} trials have a standard error of {:.3}, and \
         {POWER_TRIALS} trials {:.3}.\n",
        (0.05f64 * 0.95 / FALSE_TRIALS as f64).sqrt(),
        (0.05f64 * 0.95 / POWER_TRIALS as f64).sqrt()
    );
    for (index, sd) in (1u64..).zip(RUN_SD_FRACTIONS) {
        report_setting(index, sd);
    }
    eprintln!("calibration_report took {:.0} s", start.elapsed().as_secs_f64());
}
