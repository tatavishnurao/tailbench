//! Calibration of `compare` on synthetic runs (see `synth`): how often each
//! `CompareMethod` names a direction when A and B are the same distribution
//! (false-verdict rate), and how often it says Slower when B really is
//! slower (power). Both methods see the same trials.
//!
//! A last section calibrates `compare_split`: its body verdict, its episode
//! verdict, and per-run p99 of all samples on the same trials.
//!
//! Report only, no assertions. Slow, so ignored; run with
//! `cargo test --release --lib calibration_report -- --ignored --nocapture`.
//! It prints Markdown, and also writes it to docs/calibration.md when the
//! cargo process has TAILBENCH_WRITE_CALIBRATION=1 in its environment.
//! `split_calibration` prints the split section alone and writes nothing.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::compare::{
    CompareMethod, DEFAULT_SEED, EpisodeOutcome, Group, RecordedRun, SplitOptions, Verdict, compare_split, compare_with,
};
use crate::episodes::{self, Threshold, ThresholdRule};
use crate::manifest::{Manifest, Probe};
use crate::stats::{self, DEFAULT_LEVEL, DEFAULT_RESAMPLES};
use crate::synth::{PoissonStalls, Stall, Synth};

const SIGMA: f64 = 0.5;
const PHIS: [f64; 2] = [0.5, 0.9];
/// Between-run sd of each setting, as a fraction of the 10 ms median.
const RUN_SD_FRACTIONS: [f64; 4] = [0.0, 0.03, 0.10, 0.20];
const STALL: Stall = Stall {
    prob: 0.2,
    extra_ns: 300e6,
    fraction: 0.02,
};

const METHODS: [CompareMethod; 2] = [CompareMethod::Hierarchical, CompareMethod::PerRunWelch];

const FALSE_RUNS: [usize; 3] = [3, 5, 10];
const FALSE_TRIALS: u64 = 300;
const FALSE_QUANTILES: [f64; 2] = [0.5, 0.99];

const POWER_RUNS: [usize; 2] = [5, 10];
const POWER_TRIALS: u64 = 100;
const POWER_SCALES: [f64; 4] = [1.02, 1.05, 1.10, 1.20];
const POWER_Q: f64 = 0.99;

/// Runs for estimating the true deltas: 1000 runs of 1000 samples per scale.
const TRUTH_RUNS: u64 = 1000;

/// The split section: every phi, these between-run sds.
const SPLIT_RUN_SD_FRACTIONS: [f64; 2] = [0.0, 0.10];
const SPLIT_RUNS: [usize; 2] = [5, 10];
const SPLIT_SCALES: [f64; 3] = [1.05, 1.10, 1.20];
/// Threshold factors compared on the same trials: the old default and the
/// current one (`episodes::DEFAULT_THRESHOLD_FACTOR`).
const SPLIT_FACTORS: [f64; 2] = [5.0, 10.0];
/// Poisson stall rates per second with the same rate in A and B.
const POISSON_EQUAL: [f64; 2] = [0.05, 0.2];
/// (A, B) Poisson stall rates per second for episode power.
const POISSON_POWER: [(f64, f64); 2] = [(0.05, 0.15), (0.2, 0.6)];
/// Seed cells of the split section start here, past every cell above.
const SPLIT_CELL: u64 = 1 << 16;

/// Nominal false-verdict rate: 1 - DEFAULT_LEVEL, split over two directions.
const NOMINAL: f64 = 0.05;

const ENV_WRITE: &str = "TAILBENCH_WRITE_CALIBRATION";
const COMMAND: &str =
    "TAILBENCH_WRITE_CALIBRATION=1 cargo test --release --lib calibration_report -- --ignored --nocapture";

#[derive(Debug, Clone, Copy)]
struct Setting {
    phi: f64,
    run_sd_fraction: f64,
    stalls: bool,
}

impl Setting {
    fn model(&self) -> Synth {
        let mut m = Synth::new(SIGMA, self.phi, self.run_sd_fraction * Synth::MEDIAN_NS);
        m.stall = self.stalls.then_some(STALL);
        m
    }

    /// "| 0.5 | 3% | no |": the setting columns of a table row.
    fn cells(&self) -> String {
        format!(
            "| {} | {:.0}% | {} |",
            self.phi,
            self.run_sd_fraction * 100.0,
            if self.stalls { "yes" } else { "no" }
        )
    }

    fn label(&self) -> String {
        format!(
            "phi {}, sd {:.0}%, {}",
            self.phi,
            self.run_sd_fraction * 100.0,
            if self.stalls { "stalls" } else { "no stalls" }
        )
    }
}

/// Every combination: between-run sd x phi x stalls.
fn settings() -> Vec<Setting> {
    let mut out = Vec::new();
    for stalls in [false, true] {
        for phi in PHIS {
            for run_sd_fraction in RUN_SD_FRACTIONS {
                out.push(Setting {
                    phi,
                    run_sd_fraction,
                    stalls,
                });
            }
        }
    }
    out
}

/// Whether a comparison said Slower or Faster.
#[derive(Debug, Clone, Copy, Default)]
struct Outcome {
    slower: bool,
    faster: bool,
}

/// At min_effect 0, so every direction the CI supports counts.
fn outcome(a: &[RecordedRun], b: &[RecordedRun], q: f64, method: CompareMethod) -> Outcome {
    let c = compare_with(a, b, q, Duration::ZERO, method);
    match c.verdict {
        Verdict::Slower { .. } => Outcome {
            slower: true,
            faster: false,
        },
        Verdict::Faster { .. } => Outcome {
            slower: false,
            faster: true,
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

/// Two-sided 95% Monte Carlo error of a rate near NOMINAL over `trials`.
fn se(trials: u64) -> f64 {
    (NOMINAL * (1.0 - NOMINAL) / trials as f64).sqrt()
}

/// Whether `hits` of `trials` is above NOMINAL by more than 2 Monte Carlo
/// standard errors.
fn significantly_high(hits: usize, trials: u64) -> bool {
    hits as f64 / trials as f64 > NOMINAL + 2.0 * se(trials)
}

/// "0.046 (23)", in bold when significantly above NOMINAL.
fn false_cell(hits: usize, trials: u64) -> String {
    let cell = format!("{:.3} ({hits})", hits as f64 / trials as f64);
    if significantly_high(hits, trials) {
        format!("**{cell}**")
    } else {
        cell
    }
}

fn power_cell(hits: usize, trials: u64) -> String {
    format!("{:.2}", hits as f64 / trials as f64)
}

fn method_label(method: CompareMethod) -> &'static str {
    match method {
        CompareMethod::Hierarchical => "Hier",
        CompareMethod::PerRunWelch => "Welch",
    }
}

/// True delta at `scale` of the two estimands, in ms: the POOLED p99
/// (Hierarchical's) and the MEAN of per-run p99s (Welch's). A and B use the
/// same runs, so the differences have little Monte Carlo noise.
fn true_deltas(model: &Synth, scale: f64) -> (f64, f64) {
    let estimands = |scale: f64| {
        let per_run: Vec<Vec<u64>> = (0..TRUTH_RUNS)
            .map(|run| {
                let s = model.samples(u64::MAX - run, scale);
                s.iter().map(|s| s.latency_ns).collect()
            })
            .collect();
        let pooled = stats::percentile(&per_run.concat(), POWER_Q).expect("non-empty") as f64;
        let mean = per_run
            .iter()
            .map(|r| stats::percentile(r, POWER_Q).expect("non-empty") as f64)
            .sum::<f64>()
            / TRUTH_RUNS as f64;
        (pooled, mean)
    };
    let ((pooled_a, mean_a), (pooled_b, mean_b)) = (estimands(1.0), estimands(scale));
    ((pooled_b - pooled_a) / 1e6, (mean_b - mean_a) / 1e6)
}

/// Rates, per method and R, collected for the summary.
#[derive(Default)]
struct Worst {
    /// (rate, where) of the highest false-verdict rate, per method and R.
    false_rate: Vec<((CompareMethod, usize), (f64, String))>,
    significant: Vec<((CompareMethod, usize), usize)>,
}

impl Worst {
    fn record(&mut self, method: CompareMethod, r: usize, hits: usize, trials: u64, at: String) {
        let rate = hits as f64 / trials as f64;
        let key = (method, r);
        match self.false_rate.iter_mut().find(|(k, _)| *k == key) {
            Some((_, best)) if best.0 >= rate => {}
            Some((_, best)) => *best = (rate, at),
            None => self.false_rate.push((key, (rate, at))),
        }
        let sig = usize::from(significantly_high(hits, trials));
        match self.significant.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += sig,
            None => self.significant.push((key, sig)),
        }
    }
}

fn false_table(out: &mut String, settings: &[Setting], m: &Manifest, worst: &mut Worst) {
    let p = stats::quantile_label;
    let mut header = String::from("| phi | between-run sd | stalls | R |");
    let mut rule = String::from("|---:|---:|:---|---:|");
    for q in FALSE_QUANTILES {
        for method in METHODS {
            header += &format!(" {}, {} |", p(q), method_label(method));
            rule += "---:|";
        }
    }
    writeln!(out, "{header}\n{rule}").unwrap();
    for (si, setting) in (0u64..).zip(settings) {
        let model = setting.model();
        for (k, r) in (0u64..).zip(FALSE_RUNS) {
            let cell = si * 100 + k;
            // per_trial[t][qi][mi]
            let per_trial: Vec<Vec<Vec<Outcome>>> = par_map(FALSE_TRIALS, |t| {
                let a = runs(&model, m, cell, t, 0, r, 1.0);
                let b = runs(&model, m, cell, t, 1, r, 1.0);
                FALSE_QUANTILES
                    .iter()
                    .map(|&q| METHODS.iter().map(|&method| outcome(&a, &b, q, method)).collect())
                    .collect()
            });
            let mut row = format!("{} {r} |", setting.cells());
            for (qi, q) in FALSE_QUANTILES.into_iter().enumerate() {
                for (mi, method) in METHODS.into_iter().enumerate() {
                    let hits = per_trial
                        .iter()
                        .filter(|o| o[qi][mi].slower || o[qi][mi].faster)
                        .count();
                    worst.record(method, r, hits, FALSE_TRIALS, format!("{}, {}", setting.label(), p(q)));
                    row += &format!(" {} |", false_cell(hits, FALSE_TRIALS));
                }
            }
            writeln!(out, "{row}").unwrap();
        }
    }
}

fn power_table(out: &mut String, settings: &[Setting], m: &Manifest) {
    let mut header =
        String::from("| phi | between-run sd | stalls | B scale | true delta, pooled | true delta, per-run mean |");
    let mut rule = String::from("|---:|---:|:---|---:|---:|---:|");
    for r in POWER_RUNS {
        for method in METHODS {
            header += &format!(" R={r}, {} |", method_label(method));
            rule += "---:|";
        }
    }
    header += " wrong way, Hier / Welch |";
    rule += "---:|";
    writeln!(out, "{header}\n{rule}").unwrap();
    for (si, setting) in (0u64..).zip(settings) {
        let model = setting.model();
        for (k, scale) in (0u64..).zip(POWER_SCALES) {
            let (pooled, mean) = true_deltas(&model, scale);
            let mut row = format!("{} {scale:.2} | {pooled:.2} ms | {mean:.2} ms |", setting.cells());
            let mut wrong = [0usize; METHODS.len()];
            for (j, r) in (0u64..).zip(POWER_RUNS) {
                let cell = si * 100 + 50 + j * 10 + k;
                // per_trial[t][mi]
                let per_trial: Vec<Vec<Outcome>> = par_map(POWER_TRIALS, |t| {
                    let a = runs(&model, m, cell, t, 0, r, 1.0);
                    let b = runs(&model, m, cell, t, 1, r, scale);
                    METHODS.iter().map(|&method| outcome(&a, &b, POWER_Q, method)).collect()
                });
                for (mi, w) in wrong.iter_mut().enumerate() {
                    *w += per_trial.iter().filter(|o| o[mi].faster).count();
                    let slower = per_trial.iter().filter(|o| o[mi].slower).count();
                    row += &format!(" {} |", power_cell(slower, POWER_TRIALS));
                }
            }
            writeln!(out, "{row} {} / {} |", wrong[0], wrong[1]).unwrap();
        }
    }
}

fn summary(out: &mut String, worst: &Worst, cells_per_key: usize) {
    writeln!(
        out,
        "| method | R | worst false-verdict rate | where | cells significantly above {NOMINAL} |\n|:---|---:|---:|:---|---:|"
    )
    .unwrap();
    for method in METHODS {
        for r in FALSE_RUNS {
            let key = (method, r);
            let (rate, at) = &worst.false_rate.iter().find(|(k, _)| *k == key).expect("recorded").1;
            let sig = worst.significant.iter().find(|(k, _)| *k == key).expect("recorded").1;
            writeln!(
                out,
                "| {} | {r} | {rate:.3} | {at} | {sig} of {cells_per_key} |",
                method_label(method)
            )
            .unwrap();
        }
    }
}

/// One `compare_split` at POWER_Q, min_effect 0: the body's direction,
/// the episodes' (More counts as slower, Fewer as faster), and k.
#[derive(Debug, Clone, Copy, Default)]
struct SplitOutcome {
    body: Outcome,
    episodes: Outcome,
    k: usize,
}

/// One trial: `compare_split` at each of SPLIT_FACTORS, and PerRunWelch on
/// all samples (`compare`).
#[derive(Debug, Clone, Copy, Default)]
struct SplitTrial {
    split: [SplitOutcome; SPLIT_FACTORS.len()],
    all: Outcome,
}

impl SplitTrial {
    /// At the default factor.
    fn default_split(&self) -> SplitOutcome {
        let i = SPLIT_FACTORS
            .iter()
            .position(|&f| f == episodes::DEFAULT_THRESHOLD_FACTOR)
            .expect("the default factor is calibrated");
        self.split[i]
    }
}

fn split_outcome(a: &[RecordedRun], b: &[RecordedRun], factor: f64) -> SplitOutcome {
    let opts = SplitOptions {
        min_effect: Duration::ZERO,
        threshold: ThresholdRule::MedianFactor(factor),
    };
    let s = compare_split(a, b, POWER_Q, opts);
    let body = match s.body.verdict {
        Verdict::Slower { .. } => Outcome {
            slower: true,
            faster: false,
        },
        Verdict::Faster { .. } => Outcome {
            slower: false,
            faster: true,
        },
        Verdict::Inconclusive { .. } => Outcome::default(),
        Verdict::Refused { .. } => panic!("synthetic runs are comparable: {s}"),
    };
    let episodes = match s.episodes.outcome {
        EpisodeOutcome::More => Outcome {
            slower: true,
            faster: false,
        },
        EpisodeOutcome::Fewer => Outcome {
            slower: false,
            faster: true,
        },
        EpisodeOutcome::Inconclusive { .. } => Outcome::default(),
        EpisodeOutcome::Refused { .. } => panic!("synthetic runs are comparable: {s}"),
    };
    let k = s.episodes.episodes(Group::A) + s.episodes.episodes(Group::B);
    SplitOutcome { body, episodes, k }
}

fn split_trial(a: &[RecordedRun], b: &[RecordedRun]) -> SplitTrial {
    SplitTrial {
        split: SPLIT_FACTORS.map(|factor| split_outcome(a, b, factor)),
        all: outcome(a, b, POWER_Q, CompareMethod::PerRunWelch),
    }
}

/// Every phi x `SPLIT_RUN_SD_FRACTIONS`, with the one-stall-per-run model
/// (STALL) of the tables above.
fn split_settings() -> Vec<Setting> {
    let mut out = Vec::new();
    for phi in PHIS {
        for run_sd_fraction in SPLIT_RUN_SD_FRACTIONS {
            out.push(Setting {
                phi,
                run_sd_fraction,
                stalls: true,
            });
        }
    }
    out
}

/// `setting`'s body with Poisson stalls at `per_s` (300 ms over 2% of the
/// run each, as STALL) instead of STALL.
fn poisson_model(setting: &Setting, per_s: f64) -> Synth {
    let mut m = setting.model();
    m.stall = None;
    m.poisson_stalls = Some(PoissonStalls {
        per_s,
        extra_ns: STALL.extra_ns,
        fraction: STALL.fraction,
    });
    m
}

fn count(trials: &[SplitTrial], f: impl Fn(&SplitTrial) -> bool) -> usize {
    trials.iter().filter(|t| f(t)).count()
}

fn any(o: &Outcome) -> bool {
    o.slower || o.faster
}

/// Mean k at the default factor.
fn mean_k(trials: &[SplitTrial]) -> f64 {
    trials.iter().map(|t| t.default_split().k as f64).sum::<f64>() / trials.len() as f64
}

/// Mean per-run p99 of `runs`, in ms.
fn mean_p99_ms(runs: &[Vec<u64>]) -> f64 {
    let sum: f64 = runs
        .iter()
        .map(|r| stats::percentile(r, POWER_Q).expect("non-empty") as f64)
        .sum();
    sum / runs.len() as f64 / 1e6
}

/// What the split does to the truth at the default factor, per setting and
/// stall model, over TRUTH_RUNS runs at scale 1 with the same seeds with
/// and without stalls: the mean per-run p99 of all samples without stalls
/// (what the body should estimate), the mean per-run body p99 without and
/// with stalls, the share of samples left out of the body, and the mean and
/// variance-to-mean ratio of episodes per run with stalls.
fn split_truth_table(out: &mut String, settings: &[Setting]) {
    writeln!(
        out,
        "| phi | between-run sd | stalls | p99, no stalls | body p99, no stalls | body p99, stalls | body bias, \
         stalls | not body, stalls | episodes/run | variance/mean |\n\
         |---:|---:|:---|---:|---:|---:|---:|---:|---:|---:|"
    )
    .unwrap();
    for setting in settings {
        let mut models: Vec<(String, Synth)> = vec![(format!("p {} per run", STALL.prob), setting.model())];
        for per_s in POISSON_EQUAL {
            models.push((format!("Poisson {per_s}/s"), poisson_model(setting, per_s)));
        }
        let mut plain = setting.model();
        plain.stall = None;
        let split = |model: &Synth| {
            let runs: Vec<Vec<crate::Sample>> = (0..TRUTH_RUNS).map(|run| model.samples(u64::MAX - run, 1.0)).collect();
            let slices: Vec<&[crate::Sample]> = runs.iter().map(Vec::as_slice).collect();
            let t = Threshold::derive(&slices, ThresholdRule::default()).expect("non-empty");
            let all: Vec<Vec<u64>> = runs.iter().map(|r| r.iter().map(|s| s.latency_ns).collect()).collect();
            let body: Vec<Vec<u64>> = runs
                .iter()
                .map(|r| episodes::body(r, t.ns).iter().map(|s| s.latency_ns).collect())
                .collect();
            let split: Vec<episodes::RunEpisodes> = runs.iter().map(|r| episodes::run_episodes(r, t.ns)).collect();
            (all, body, split)
        };
        let (all_plain, body_plain, _) = split(&plain);
        let truth = mean_p99_ms(&all_plain);
        for (label, model) in &models {
            let (_, body_stalled, per_run) = split(model);
            let body_ms = mean_p99_ms(&body_stalled);
            let counts: Vec<f64> = per_run.iter().map(|r| r.episodes as f64).collect();
            let n = counts.len() as f64;
            let mean = counts.iter().sum::<f64>() / n;
            let var = counts.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / (n - 1.0);
            let excluded = per_run.iter().map(|r| r.excluded).sum::<usize>() as f64 / (n * model.n as f64);
            writeln!(
                out,
                "| {} | {:.0}% | {label} | {truth:.2} ms | {:.2} ms | {body_ms:.2} ms | {:+.1}% | {:.1}% | {mean:.2} | \
                 {:.2} |",
                setting.phi,
                setting.run_sd_fraction * 100.0,
                mean_p99_ms(&body_plain),
                (body_ms / truth - 1.0) * 100.0,
                excluded * 100.0,
                var / mean,
            )
            .unwrap();
        }
    }
}

/// "| R=5, factor 5 | R=5, factor 10 |" style headers.
fn factor_header(r: usize, what: &str) -> String {
    SPLIT_FACTORS
        .iter()
        .map(|f| format!(" R={r}, {what}, factor {f} |"))
        .collect()
}

/// Same distribution on both sides (STALL in each): how often the body,
/// the episodes and per-run p99 of all samples name a direction, at each
/// factor.
fn split_false_table(out: &mut String, settings: &[Setting], m: &Manifest) {
    let mut header = String::from("| phi | between-run sd | R |");
    let mut rule = String::from("|---:|---:|---:|");
    for what in ["body", "episodes"] {
        for f in SPLIT_FACTORS {
            header += &format!(" {what}, factor {f} |");
            rule += "---:|";
        }
    }
    header += " per-run p99, all samples |";
    rule += "---:|";
    writeln!(out, "{header}\n{rule}").unwrap();
    for (si, setting) in (0u64..).zip(settings) {
        let model = setting.model();
        for (k, r) in (0u64..).zip(SPLIT_RUNS) {
            let cell = SPLIT_CELL + si * 10 + k;
            let trials = par_map(FALSE_TRIALS, |t| {
                split_trial(
                    &runs(&model, m, cell, t, 0, r, 1.0),
                    &runs(&model, m, cell, t, 1, r, 1.0),
                )
            });
            let mut row = format!("| {} | {:.0}% | {r} |", setting.phi, setting.run_sd_fraction * 100.0);
            for part in [|o: &SplitOutcome| any(&o.body), |o: &SplitOutcome| any(&o.episodes)] {
                for fi in 0..SPLIT_FACTORS.len() {
                    row += &format!(
                        " {} |",
                        false_cell(count(&trials, |t| part(&t.split[fi])), FALSE_TRIALS)
                    );
                }
            }
            row += &format!(" {} |", false_cell(count(&trials, |t| any(&t.all)), FALSE_TRIALS));
            writeln!(out, "{row}").unwrap();
        }
    }
}

/// B's body slower by each scale, STALL on both sides, the same trials for
/// both tables: the body's power at each factor against per-run p99 of all
/// samples, and how often the episode verdict says More (the leak).
fn split_power_tables(power: &mut String, leak: &mut String, settings: &[Setting], m: &Manifest) {
    let mut power_header = String::from("| phi | between-run sd | B scale |");
    let mut leak_header = power_header.clone();
    let mut power_rule = String::from("|---:|---:|---:|");
    let mut leak_rule = power_rule.clone();
    for r in SPLIT_RUNS {
        power_header += &factor_header(r, "body");
        power_header += &format!(" R={r}, per-run p99 |");
        power_rule += &"---:|".repeat(SPLIT_FACTORS.len() + 1);
        leak_header += &factor_header(r, "More");
        leak_rule += &"---:|".repeat(SPLIT_FACTORS.len());
    }
    power_header += " body wrong way, by factor |";
    power_rule += "---:|";
    writeln!(power, "{power_header}\n{power_rule}").unwrap();
    writeln!(leak, "{leak_header}\n{leak_rule}").unwrap();
    for (si, setting) in (0u64..).zip(settings) {
        let model = setting.model();
        for (k, scale) in (0u64..).zip(SPLIT_SCALES) {
            let cells = format!(
                "| {} | {:.0}% | {scale:.2} |",
                setting.phi,
                setting.run_sd_fraction * 100.0
            );
            let (mut power_row, mut leak_row) = (cells.clone(), cells);
            let mut wrong = [0usize; SPLIT_FACTORS.len()];
            for (j, r) in (0u64..).zip(SPLIT_RUNS) {
                let cell = SPLIT_CELL + 1000 + si * 100 + j * 10 + k;
                let trials = par_map(POWER_TRIALS, |t| {
                    split_trial(
                        &runs(&model, m, cell, t, 0, r, 1.0),
                        &runs(&model, m, cell, t, 1, r, scale),
                    )
                });
                for (fi, w) in wrong.iter_mut().enumerate() {
                    *w += count(&trials, |t| t.split[fi].body.faster);
                    power_row += &format!(
                        " {} |",
                        power_cell(count(&trials, |t| t.split[fi].body.slower), POWER_TRIALS)
                    );
                    leak_row += &format!(
                        " {} |",
                        power_cell(count(&trials, |t| t.split[fi].episodes.slower), POWER_TRIALS)
                    );
                }
                power_row += &format!(" {} |", power_cell(count(&trials, |t| t.all.slower), POWER_TRIALS));
            }
            let wrong: Vec<String> = wrong.iter().map(ToString::to_string).collect();
            writeln!(power, "{power_row} {} |", wrong.join(" / ")).unwrap();
            writeln!(leak, "{leak_row}").unwrap();
        }
    }
}

/// Poisson stalls at `(per_s_a, per_s_b)`, bodies equal, `trials` per row,
/// at the default factor: how often the episodes say More and Fewer, how
/// often the body names a direction, and mean k.
fn poisson_table(
    out: &mut String,
    settings: &[Setting],
    m: &Manifest,
    rates: &[(f64, f64)],
    trials: u64,
    cell_base: u64,
    rate: fn(usize, u64) -> String,
) {
    writeln!(
        out,
        "| phi | between-run sd | stalls/s A, B | R | episodes More | episodes Fewer | body Slower or Faster | mean k \
         |\n|---:|---:|---:|---:|---:|---:|---:|---:|"
    )
    .unwrap();
    for (si, setting) in (0u64..).zip(settings) {
        for (li, &(per_s_a, per_s_b)) in (0u64..).zip(rates) {
            let (model_a, model_b) = (poisson_model(setting, per_s_a), poisson_model(setting, per_s_b));
            for (k, r) in (0u64..).zip(SPLIT_RUNS) {
                let cell = cell_base + si * 100 + li * 10 + k;
                let results = par_map(trials, |t| {
                    split_trial(
                        &runs(&model_a, m, cell, t, 0, r, 1.0),
                        &runs(&model_b, m, cell, t, 1, r, 1.0),
                    )
                });
                writeln!(
                    out,
                    "| {} | {:.0}% | {per_s_a}, {per_s_b} | {r} | {} | {} | {} | {:.1} |",
                    setting.phi,
                    setting.run_sd_fraction * 100.0,
                    rate(count(&results, |t| t.default_split().episodes.slower), trials),
                    rate(count(&results, |t| t.default_split().episodes.faster), trials),
                    rate(count(&results, |t| any(&t.default_split().body)), trials),
                    mean_k(&results),
                )
                .unwrap();
            }
        }
    }
}

/// The "Split verdict" section: `compare_split` at POWER_Q.
fn split_section(m: &Manifest) -> String {
    let settings = split_settings();
    let p = stats::quantile_label(POWER_Q);
    let factors: Vec<String> = SPLIT_FACTORS.iter().map(ToString::to_string).collect();
    let mut out = String::new();
    writeln!(
        out,
        "## Split verdict\n\n\
         `compare_split` at {p}, min_effect 0: every run is split at T = factor x the median of all A and B samples \
         pooled (default factor {default}; tables that compare factors show {factors}). The body (samples sent \
         outside every episode window) is compared by PerRunWelch at {p}, without the episode guard; the episodes \
         by the exact conditional binomial test on their rate per unit of run time (two-sided, p < {NOMINAL}, \
         k >= 3, R >= 3). \"Per-run p99, all samples\" is `compare` (PerRunWelch, with its episode guard) on the \
         same trials.\n\n\
         Two stall models: \"p {prob} per run\", the one of the tables above (each run stalls at most once, with \
         probability {prob}); and Poisson stalls in time at a rate per second, any number per run, each adding \
         {extra:.0} ms to {frac:.0}% of the run from its start (overlapping stalls merge).\n\n\
         ### What the split estimates\n\n\
         At the default factor, {TRUTH_RUNS} runs per row at scale 1, the same seeds with and without stalls. \
         \"Body bias\" is the mean per-run body {p} with stalls against the mean per-run {p} of all samples without \
         stalls. \"Not body\" is the share of samples sent inside an episode window. Episodes per run counts stalls \
         (overlapping ones as one) and tail excursions of the body above T; variance/mean is 1 for a Poisson \
         count.\n",
        default = episodes::DEFAULT_THRESHOLD_FACTOR,
        factors = factors.join(" and "),
        prob = STALL.prob,
        extra = STALL.extra_ns / 1e6,
        frac = STALL.fraction * 100.0,
    )
    .unwrap();
    split_truth_table(&mut out, &settings);

    writeln!(
        out,
        "\n### False-verdict rate, one stall per run\n\n\
         A and B from the same distribution, stalls p {} per run, {FALSE_TRIALS} trials per row; cells are rate \
         (count), **bold** above {NOMINAL} + 2 SE = {:.3}.\n",
        STALL.prob,
        NOMINAL + 2.0 * se(FALSE_TRIALS)
    )
    .unwrap();
    split_false_table(&mut out, &settings, m);

    let (mut power, mut leak) = (String::new(), String::new());
    split_power_tables(&mut power, &mut leak, &settings, m);
    writeln!(
        out,
        "\n### Leak: body regression reported as more episodes\n\n\
         B's body multiplied by the scale (stalls excepted), stalls p {} per run on both sides, {POWER_TRIALS} \
         trials per cell; cells are the fraction of trials whose episode verdict says More. Every one is a false \
         verdict: the stall rate is the same. Target at the default factor: <= {NOMINAL}.\n\n{leak}",
        STALL.prob
    )
    .unwrap();
    writeln!(
        out,
        "### Body power\n\n\
         The same trials; cells are the fraction with body Slower, and per-run {p} of all samples Slower. \"Body \
         wrong way\" counts body Faster verdicts over both R, per factor.\n\n{power}"
    )
    .unwrap();

    writeln!(
        out,
        "### Episode false-verdict rate, Poisson stalls\n\n\
         The same stall rate in A and B, bodies equal, {FALSE_TRIALS} trials per row, default factor; rate \
         (count), **bold** above {:.3}. Mean k is A's and B's episodes together.\n",
        NOMINAL + 2.0 * se(FALSE_TRIALS)
    )
    .unwrap();
    let equal: Vec<(f64, f64)> = POISSON_EQUAL.iter().map(|&r| (r, r)).collect();
    poisson_table(
        &mut out,
        &settings,
        m,
        &equal,
        FALSE_TRIALS,
        SPLIT_CELL + 3000,
        false_cell,
    );

    writeln!(
        out,
        "\n### Episode power, Poisson stalls\n\n\
         B's stall rate 3x A's, bodies equal, {POWER_TRIALS} trials per row, default factor; cells are fractions \
         of trials.\n"
    )
    .unwrap();
    poisson_table(
        &mut out,
        &settings,
        m,
        &POISSON_POWER,
        POWER_TRIALS,
        SPLIT_CELL + 4000,
        power_cell,
    );
    out
}

/// "2026-10-19" (UTC) for nanoseconds since the Unix epoch, by Howard
/// Hinnant's days-to-civil algorithm.
fn utc_date(unix_ns: u128) -> String {
    let days = (unix_ns / 86_400_000_000_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn probe<T: ToString>(p: &Probe<T>) -> String {
    match p {
        Probe::Known(v) => v.to_string(),
        Probe::Unknown => "UNKNOWN".to_owned(),
    }
}

fn header(m: &Manifest, elapsed: Duration) -> String {
    let date = match m.start_unix_ns {
        Probe::Known(ns) => utc_date(ns),
        Probe::Unknown => "UNKNOWN".to_owned(),
    };
    let dirty = match m.runtime_git_dirty {
        Probe::Known(true) => " (working tree dirty: uncommitted changes were included)",
        Probe::Known(false) => "",
        Probe::Unknown => " (working tree state UNKNOWN)",
    };
    format!(
        "# Calibration of compare\n\n\
         Generated by `calibration_report` in src/calibration.rs; do not edit by hand.\n\n\
         - Date: {date} (UTC)\n\
         - Git SHA: {}{dirty}\n\
         - Machine: {}; {}; kernel {}; {} {}\n\
         - Build: {}, profile {}, opt-level {}\n\
         - Runtime: {:.0} s\n\
         - Regenerate: `{COMMAND}`\n\n",
        probe(&m.runtime_git_sha),
        m.machine_label,
        probe(&m.cpu_model),
        probe(&m.kernel),
        m.os,
        m.arch,
        m.rust_version,
        probe(&m.build_profile),
        probe(&m.opt_level),
        elapsed.as_secs_f64(),
    )
}

#[test]
#[ignore = "slow: about 5 minutes in --release; run with --release --ignored --nocapture"]
fn calibration_report() {
    let start = Instant::now();
    let settings = settings();
    // The real manifest, for the header; runs use the synth one, git clean.
    let real = Manifest::collect(&Synth::new(SIGMA, PHIS[0], 0.0).config());
    let m = Synth::new(SIGMA, PHIS[0], 0.0).manifest();

    let mut body = String::new();
    writeln!(
        body,
        "## Setup\n\n\
         Synthetic runs (src/synth.rs), nanosecond resolution, {} samples per run at 10 ms intervals. Latency = \
         10 ms x exp({SIGMA} x x_i) + offset, where x_i is a stationary AR(1) series with lag-1 correlation phi, \
         offset ~ N(0, sd^2) is drawn once per run (between-run sd, as a % of the median), and the result is \
         multiplied by B's scale. With stalls, each run independently stalls with probability {}: {:.0} ms is \
         added (unscaled) to every sample of one contiguous {:.0}% of the run.\n\n\
         Methods, on the same trials:\n\n\
         - Hier: `CompareMethod::Hierarchical`, delta of the pooled percentile, hierarchical bootstrap \
         ({DEFAULT_RESAMPLES} resamples, seed {DEFAULT_SEED:#018X}), percentile interval.\n\
         - Welch: `CompareMethod::PerRunWelch`, delta of the mean per-run percentile, Welch t interval with \
         Welch-Satterthwaite df.\n\n\
         Both at nominal {}%, min_effect 0 (so the CI alone decides), with the same episode guard and R >= 3 rule.\n",
        m.count,
        STALL.prob,
        STALL.extra_ns / 1e6,
        STALL.fraction * 100.0,
        DEFAULT_LEVEL * 100.0,
    )
    .unwrap();

    writeln!(
        body,
        "## False-verdict rate\n\n\
         A and B from the same distribution, {FALSE_TRIALS} trials per row; a false verdict is Slower or Faster, \
         target <= {NOMINAL}. Cells are rate (count). Monte Carlo SE at a rate of {NOMINAL} is {:.3}; **bold** \
         marks a rate above {NOMINAL} + 2 SE = {:.3}.\n",
        se(FALSE_TRIALS),
        NOMINAL + 2.0 * se(FALSE_TRIALS)
    )
    .unwrap();
    let mut worst = Worst::default();
    false_table(&mut body, &settings, &m, &mut worst);
    eprintln!("false-verdict table done at {:.0} s", start.elapsed().as_secs_f64());

    writeln!(
        body,
        "\n## Power at {}\n\n\
         B's latency multiplied by the scale (stalls excepted), {POWER_TRIALS} trials per cell; cells are the \
         fraction of trials with verdict Slower. Monte Carlo SE of a power p is sqrt(p (1 - p) / {POWER_TRIALS}), \
         at most {:.3}. True deltas are from {TRUTH_RUNS} runs per side with common seeds: \"pooled\" is \
         Hier's estimand, \"per-run mean\" is Welch's. \"Wrong way\" counts Faster verdicts over both R.\n",
        stats::quantile_label(POWER_Q),
        (0.25 / POWER_TRIALS as f64).sqrt()
    )
    .unwrap();
    power_table(&mut body, &settings, &m);

    writeln!(
        body,
        "\n## Summary\n\nWorst false-verdict rate per method and R, over every setting and both percentiles.\n"
    )
    .unwrap();
    summary(&mut body, &worst, settings.len() * FALSE_QUANTILES.len());
    writeln!(body).unwrap();
    body += &split_section(&m);

    let report = header(&real, start.elapsed()) + &body;
    println!("{report}");
    if std::env::var(ENV_WRITE).as_deref() == Ok("1") {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/calibration.md");
        std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/docs")).expect("create docs/");
        std::fs::write(path, &report).expect("write docs/calibration.md");
        eprintln!("wrote {path}");
    } else {
        eprintln!("{ENV_WRITE} is not 1: docs/calibration.md not written");
    }
    eprintln!("calibration_report took {:.0} s", start.elapsed().as_secs_f64());
}

/// The split section alone, printed; writes nothing.
#[test]
#[ignore = "slow: under a minute in --release; run with --release --ignored --nocapture"]
fn split_calibration() {
    let start = Instant::now();
    let m = Synth::new(SIGMA, PHIS[0], 0.0).manifest();
    println!("{}", split_section(&m));
    eprintln!("split_calibration took {:.0} s", start.elapsed().as_secs_f64());
}

#[test]
fn utc_date_known_days() {
    assert_eq!(utc_date(0), "1970-01-01");
    // 2000-02-29 00:00:00 UTC = 951782400 s; 2026-10-19 = 1792368000 s.
    assert_eq!(utc_date(951_782_400 * 1_000_000_000), "2000-02-29");
    assert_eq!(
        utc_date(1_792_368_000 * 1_000_000_000 + 86_399_999_999_999),
        "2026-10-19"
    );
}
