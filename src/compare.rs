//! A/B verdict: is B slower or faster than A at one percentile, over
//! repeated runs of each?
//!
//! This file exists so a verdict only names a direction when the data can
//! carry it. Everything that can make "B is slower" untrue or unsupported
//! becomes an inconclusive reason (too few runs, a tail made of too few
//! separate episodes, a CI of the difference that crosses 0, an effect below
//! the caller's threshold), a refusal (runs that cannot be compared), or a
//! warning printed with the verdict (environments that differ).

use std::fmt;
use std::time::Duration;

use crate::manifest::{Manifest, Probe};
use crate::stats::{self, CiWarning, DEFAULT_LEVEL, DEFAULT_RESAMPLES, MIN_EPISODES, MIN_RUNS, Pooled, SplitMix64};
use crate::{RunResult, RunnerKind, Sample};

/// Bootstrap seed `compare` uses, fixed so a comparison is deterministic.
/// A's bootstrap uses it; B's uses the first output of SplitMix64 seeded
/// with it, so the two groups' resamples are independent even when both
/// have the same shape.
pub const DEFAULT_SEED: u64 = 0xA5EE_D000_0000_000A;

/// Default `min_effect` for `compare`: 1 ms, tokio's timer granularity.
/// Open-loop sends wait on `tokio::time::sleep_until`, which tokio 1.53.3
/// documents as "`Sleep` operates at millisecond granularity", so every
/// latency includes up to about 1 ms of timer lag (more on Windows). A
/// difference smaller than that can come from the timer, not the target.
/// Pass a larger value for a slower timer; pass `Duration::ZERO` only for
/// data that is not timed by tokio.
pub const DEFAULT_MIN_EFFECT: Duration = Duration::from_millis(1);

/// How `compare` puts a CI on delta. The refusals, the warnings, the
/// `MIN_RUNS` rule, the episode guard and `min_effect` are the same for
/// both; they differ in what delta is and how its interval is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompareMethod {
    /// delta = the percentile of B's runs pooled minus that of A's. The CI
    /// is the percentile interval of paired `stats::hier_ci` bootstrap
    /// draws (`DEFAULT_RESAMPLES`, `DEFAULT_SEED`). Runs weigh by length.
    #[default]
    Hierarchical,
    /// Each run's percentile is one observation, so each run weighs the
    /// same: delta = the mean of B's per-run percentiles minus the mean of
    /// A's, and the CI is Welch's t interval with Welch-Satterthwaite df
    /// (`stats::welch_ci`). No resampling: `Comparison::resamples` and
    /// `seed` do not apply.
    PerRunWelch,
}

/// One run's result with the manifest written for it.
#[derive(Debug)]
pub struct RecordedRun {
    pub result: RunResult,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    A,
    B,
}

/// What the runs support about delta = q(B) - q(A). Values in nanoseconds.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// delta > 0, its CI [lo, hi] excludes 0, and |delta| >= min_effect.
    Slower { delta: i64, lo: i64, hi: i64 },
    /// delta < 0, its CI [lo, hi] excludes 0, and |delta| >= min_effect.
    Faster { delta: i64, lo: i64, hi: i64 },
    /// The CIs were computed but do not support a direction; `reason` says
    /// every rule that failed, with delta and its CI.
    Inconclusive { reason: String },
    /// The runs cannot be compared; no CI was computed.
    Refused { reason: String },
}

/// Something that does not stop a verdict but should be read with it.
#[derive(Debug, Clone, PartialEq)]
pub enum CompareWarning {
    /// A warning on one group's `hier_ci`.
    Ci { group: Group, warning: CiWarning },
    /// A manifest field takes more than one value across all runs. `a` and
    /// `b` are the distinct values in each group, sorted.
    ManifestDiffers {
        field: &'static str,
        a: Vec<String>,
        b: Vec<String>,
    },
    /// Runs whose `build_git_dirty` is true: the binary is not its commit.
    GitDirty { a: usize, b: usize },
    /// Runs whose `sha_mismatch` is true: the checkout moved after the build.
    ShaMismatch { a: usize, b: usize },
    /// `interval_ns` or `count` takes more than one value across all runs,
    /// so the runs were not offered the same load: a different request rate
    /// changes queueing, and a different count changes how long the load
    /// lasted. `a` and `b` are the distinct values in each group, sorted.
    OfferedLoadDiffers {
        field: &'static str,
        a: Vec<String>,
        b: Vec<String>,
    },
}

/// A verdict with what it was computed from.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    pub verdict: Verdict,
    pub q: f64,
    /// R for each group.
    pub runs_a: usize,
    pub runs_b: usize,
    /// Nominal confidence level of the delta CI.
    pub level: f64,
    pub method: CompareMethod,
    /// Hierarchical only: bootstrap resamples per group, and the seed (see
    /// `DEFAULT_SEED`). Rerunning `compare` on the same runs reproduces the
    /// same CI.
    pub resamples: usize,
    pub seed: u64,
    /// PerRunWelch only: the Welch-Satterthwaite df of the CI. `None` for
    /// Hierarchical, a refusal, or a group with fewer than 2 runs.
    pub df: Option<f64>,
    pub warnings: Vec<CompareWarning>,
}

/// Compares the `q` percentile of latency between runs `a` and `b`, by the
/// default `CompareMethod` (Hierarchical): `compare_with` with that method.
pub fn compare(a: &[RecordedRun], b: &[RecordedRun], q: f64, min_effect: Duration) -> Comparison {
    compare_with(a, b, q, min_effect, CompareMethod::default())
}

/// Compares the `q` percentile of latency between runs `a` and `b`.
///
/// Hierarchical: each group gets a `stats::hier_ci` bootstrap
/// (`DEFAULT_RESAMPLES`, `DEFAULT_SEED`); runs are pooled weighted by their
/// length (see `stats::hier_ci`). Resample i of B minus resample i of A
/// gives the bootstrap distribution of delta, and its percentile interval
/// at `DEFAULT_LEVEL` is the CI. PerRunWelch: see `CompareMethod`.
///
/// The episode guard and the CI warnings are those of `stats::hier_ci` on
/// each group, whichever the method: episodes are counted at the POOLED
/// percentile. The verdict is Slower or Faster only if none of these holds:
/// - either group has fewer than `MIN_RUNS` runs;
/// - either group's estimate is episode-limited (fewer than `MIN_EPISODES`
///   episodes at or above it); a sample-count shortfall is only a warning;
/// - there is no CI (PerRunWelch with fewer than 2 runs in a group), or
///   the CI of delta contains 0, or does not contain delta itself;
/// - |delta| < `min_effect` (usually `DEFAULT_MIN_EFFECT`).
///
/// Refused, with no CI, if a group is empty, a run has no samples, runners
/// differ, or any run is closed-loop.
pub fn compare_with(
    a: &[RecordedRun],
    b: &[RecordedRun],
    q: f64,
    min_effect: Duration,
    method: CompareMethod,
) -> Comparison {
    stats::check_quantile(q);
    let mut warnings = manifest_warnings(a, b);
    let (verdict, df) = match refusal(a, b) {
        Some(reason) => (Verdict::Refused { reason }, None),
        None => decide(a, b, q, min_effect, method, &mut warnings),
    };
    Comparison {
        verdict,
        q,
        runs_a: a.len(),
        runs_b: b.len(),
        level: DEFAULT_LEVEL,
        method,
        resamples: DEFAULT_RESAMPLES,
        seed: DEFAULT_SEED,
        df,
        warnings,
    }
}

/// Every reason the runs cannot be compared, joined, or `None`.
fn refusal(a: &[RecordedRun], b: &[RecordedRun]) -> Option<String> {
    let mut reasons = Vec::new();
    for (group, runs) in [(Group::A, a), (Group::B, b)] {
        let g = group_label(group);
        if runs.is_empty() {
            reasons.push(format!("{g} has no runs"));
        }
        let empty = runs.iter().filter(|r| r.result.samples.is_empty()).count();
        if empty > 0 {
            reasons.push(format!("{empty} {g} runs have no samples"));
        }
    }
    let all = || a.iter().chain(b);
    let first = all().next().map(|r| r.manifest.runner);
    if all().any(|r| Some(r.manifest.runner) != first) {
        reasons.push("runners differ: open-loop and closed-loop latencies measure different things".to_owned());
    }
    // A closed-loop result has no service split, whatever its manifest says.
    let closed = |runs: &[RecordedRun]| {
        runs.iter()
            .filter(|r| r.manifest.runner == RunnerKind::ClosedLoop || r.result.service.is_none())
            .count()
    };
    let (closed_a, closed_b) = (closed(a), closed(b));
    if closed_a + closed_b > 0 {
        reasons.push(format!(
            "closed-loop runs (A: {closed_a}, B: {closed_b}) measure latency from the actual send, so coordinated \
             omission hides stalls; rerun open-loop"
        ));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

/// `b - a` in nanoseconds, saturated to i64.
fn diff(b: u64, a: u64) -> i64 {
    let d = i128::from(b) - i128::from(a);
    i64::try_from(d).unwrap_or(if d < 0 { i64::MIN } else { i64::MAX })
}

fn samples(runs: &[RecordedRun]) -> Vec<&[Sample]> {
    runs.iter().map(|r| r.result.samples.as_slice()).collect()
}

/// Nanoseconds in f64 rounded to i64 (`as` saturates).
fn round_ns(ns: f64) -> i64 {
    ns.round() as i64
}

/// Hierarchical: delta of the pooled estimates, and the percentile interval
/// of paired bootstrap draws.
fn hierarchical_delta(pooled_a: &Pooled, pooled_b: &Pooled, q: f64) -> (i64, Option<(i64, i64)>) {
    let boot_a = pooled_a.hier_draws(q, DEFAULT_RESAMPLES, DEFAULT_SEED);
    let boot_b = pooled_b.hier_draws(q, DEFAULT_RESAMPLES, SplitMix64::new(DEFAULT_SEED).next_u64());
    // A's and B's resamples are independent, so pairing them by index gives
    // draws of q(B*) - q(A*).
    let mut deltas: Vec<i64> = boot_b.iter().zip(&boot_a).map(|(&b, &a)| diff(b, a)).collect();
    deltas.sort_unstable();
    let (lo_rank, hi_rank) = stats::interval_ranks(DEFAULT_RESAMPLES, DEFAULT_LEVEL);
    let delta = diff(pooled_b.estimate, pooled_a.estimate);
    (delta, Some((deltas[lo_rank - 1], deltas[hi_rank - 1])))
}

/// PerRunWelch: delta of the means of per-run percentiles, its Welch
/// interval, and the interval's df.
fn welch_delta(pooled_a: &Pooled, pooled_b: &Pooled, q: f64) -> (i64, Option<(i64, i64)>, Option<f64>) {
    let per_run = |p: &Pooled| -> Vec<f64> {
        p.latencies
            .iter()
            .map(|run| stats::percentile(run, q).expect("runs are not empty") as f64)
            .collect()
    };
    let (qa, qb) = (per_run(pooled_a), per_run(pooled_b));
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len() as f64;
    match stats::welch_ci(&qa, &qb, DEFAULT_LEVEL) {
        Some(ci) => (
            round_ns(ci.delta),
            Some((round_ns(ci.lo), round_ns(ci.hi))),
            Some(ci.df),
        ),
        None => (round_ns(mean(&qb) - mean(&qa)), None, None),
    }
}

/// The verdict for runs that passed `refusal`, and the CI's df for
/// PerRunWelch; adds each group's CI warnings to `warnings`.
fn decide(
    a: &[RecordedRun],
    b: &[RecordedRun],
    q: f64,
    min_effect: Duration,
    method: CompareMethod,
    warnings: &mut Vec<CompareWarning>,
) -> (Verdict, Option<f64>) {
    let pooled = |runs| Pooled::new(&samples(runs), q).expect("refusal rules out empty groups and runs");
    let (pooled_a, pooled_b) = (pooled(a), pooled(b));
    for (group, p) in [(Group::A, &pooled_a), (Group::B, &pooled_b)] {
        warnings.extend(p.warnings.iter().map(|w| CompareWarning::Ci {
            group,
            warning: w.clone(),
        }));
    }
    let (delta, ci, df) = match method {
        CompareMethod::Hierarchical => {
            let (delta, ci) = hierarchical_delta(&pooled_a, &pooled_b, q);
            (delta, ci, None)
        }
        CompareMethod::PerRunWelch => welch_delta(&pooled_a, &pooled_b, q),
    };

    let p = stats::quantile_label(q);
    let d = decimals_for(delta);
    let (runs_a, runs_b) = (pooled_a.latencies.len(), pooled_b.latencies.len());
    let mut reasons = Vec::new();
    if runs_a < MIN_RUNS || runs_b < MIN_RUNS {
        reasons.push(format!(
            "R < {MIN_RUNS} (A: {runs_a}, B: {runs_b}): between-run variance unknown"
        ));
    }
    if pooled_a.episodes < MIN_EPISODES || pooled_b.episodes < MIN_EPISODES {
        reasons.push(format!(
            "episode-limited: {} tail episodes at or above A's {p} ({} ms) and {} at or above B's ({} ms), want >= \
             {MIN_EPISODES} each",
            pooled_a.episodes,
            unsigned_ms(diff(pooled_a.estimate, 0), d),
            pooled_b.episodes,
            unsigned_ms(diff(pooled_b.estimate, 0), d)
        ));
    }
    match ci {
        None => reasons.push("no CI of delta: Welch needs >= 2 runs per group".to_owned()),
        Some((lo, hi)) if lo <= 0 && hi >= 0 => reasons.push("CI of delta crosses 0".to_owned()),
        // Possible with a percentile interval, if rare: it is not centred
        // on the estimate.
        Some((lo, _)) if (lo > 0) != (delta > 0) => {
            reasons.push("CI of delta excludes 0 on the other side of delta".to_owned());
        }
        Some(_) => {}
    }
    let min_ns = i64::try_from(min_effect.as_nanos()).unwrap_or(i64::MAX);
    if delta.unsigned_abs() < min_ns.unsigned_abs() {
        reasons.push(format!(
            "effect below min_effect: |delta| < {} ms",
            unsigned_ms(min_ns, decimals_for(min_ns))
        ));
    }

    let verdict = match ci {
        Some((lo, hi)) if reasons.is_empty() => {
            if delta > 0 {
                Verdict::Slower { delta, lo, hi }
            } else {
                Verdict::Faster { delta, lo, hi }
            }
        }
        _ => {
            let numbers = match ci {
                Some((lo, hi)) => format!(
                    "delta {} ms [{}, {}]",
                    signed_ms(delta, d),
                    signed_ms(lo, d),
                    signed_ms(hi, d)
                ),
                None => format!("delta {} ms, no CI", signed_ms(delta, d)),
            };
            Verdict::Inconclusive {
                reason: format!("{} ({numbers})", reasons.join("; ")),
            }
        }
    };
    (verdict, df)
}

/// `p` as a string, "UNKNOWN" when it is Unknown.
fn probe<T: ToString>(p: &Probe<T>) -> String {
    match p {
        Probe::Known(v) => v.to_string(),
        Probe::Unknown => "UNKNOWN".to_owned(),
    }
}

/// The manifest fields that change what a latency means; if they differ,
/// a difference in latency may be the environment, not the code.
fn environment(m: &Manifest) -> [(&'static str, String); 6] {
    [
        ("build_profile", probe(&m.build_profile)),
        ("opt_level", probe(&m.opt_level)),
        ("cpu_model", probe(&m.cpu_model)),
        ("kernel", probe(&m.kernel)),
        ("runtime_flavor", probe(&m.runtime_flavor)),
        ("runtime_workers", probe(&m.runtime_workers)),
    ]
}

/// The manifest fields that set the load the target was offered.
fn offered_load(m: &Manifest) -> [(&'static str, String); 2] {
    [
        ("interval_ns", m.interval_ns.to_string()),
        ("count", m.count.to_string()),
    ]
}

/// For each field `fields` reads, `(name, A's values, B's values)` if it
/// takes more than one value across all runs. Values are distinct and sorted.
fn differing<const N: usize>(
    a: &[RecordedRun],
    b: &[RecordedRun],
    fields: fn(&Manifest) -> [(&'static str, String); N],
) -> Vec<(&'static str, Vec<String>, Vec<String>)> {
    let values = |runs: &[RecordedRun], field: usize| {
        let mut v: Vec<String> = runs.iter().map(|r| fields(&r.manifest)[field].1.clone()).collect();
        v.sort();
        v.dedup();
        v
    };
    let Some(first) = a.iter().chain(b).next() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (field, (name, _)) in fields(&first.manifest).into_iter().enumerate() {
        let (va, vb) = (values(a, field), values(b, field));
        let mut all = [va.as_slice(), vb.as_slice()].concat();
        all.sort();
        all.dedup();
        if all.len() > 1 {
            out.push((name, va, vb));
        }
    }
    out
}

/// Environment fields and offered load that take more than one value across
/// all runs (an Unknown counts as a value of its own), then git state.
fn manifest_warnings(a: &[RecordedRun], b: &[RecordedRun]) -> Vec<CompareWarning> {
    let mut warnings: Vec<CompareWarning> = differing(a, b, environment)
        .into_iter()
        .map(|(field, a, b)| CompareWarning::ManifestDiffers { field, a, b })
        .collect();
    warnings.extend(
        differing(a, b, offered_load)
            .into_iter()
            .map(|(field, a, b)| CompareWarning::OfferedLoadDiffers { field, a, b }),
    );
    let count = |runs: &[RecordedRun], f: fn(&Manifest) -> &Probe<bool>| {
        runs.iter().filter(|r| *f(&r.manifest) == Probe::Known(true)).count()
    };
    let (dirty_a, dirty_b) = (count(a, |m| &m.build_git_dirty), count(b, |m| &m.build_git_dirty));
    if dirty_a + dirty_b > 0 {
        warnings.push(CompareWarning::GitDirty { a: dirty_a, b: dirty_b });
    }
    let (mismatch_a, mismatch_b) = (count(a, |m| &m.sha_mismatch), count(b, |m| &m.sha_mismatch));
    if mismatch_a + mismatch_b > 0 {
        warnings.push(CompareWarning::ShaMismatch {
            a: mismatch_a,
            b: mismatch_b,
        });
    }
    warnings
}

/// "+1.5": nanoseconds as milliseconds with a sign, to `decimals`.
fn signed_ms(ns: i64, decimals: usize) -> String {
    format!("{:+.*}", decimals, ns as f64 / 1e6)
}

fn unsigned_ms(ns: i64, decimals: usize) -> String {
    format!("{:.*}", decimals, ns as f64 / 1e6)
}

/// 1 decimal at or above 1 ms and 3 below, so a sub-millisecond effect does
/// not print as 0.0.
fn decimals_for(delta: i64) -> usize {
    if delta.unsigned_abs() >= 1_000_000 { 1 } else { 3 }
}

fn group_label(group: Group) -> &'static str {
    match group {
        Group::A => "A",
        Group::B => "B",
    }
}

impl fmt::Display for CompareWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompareWarning::Ci { group, warning } => write!(f, "{}: {warning}", group_label(*group)),
            CompareWarning::ManifestDiffers { field, a, b } => {
                write!(f, "{field} differs (A: {}; B: {})", a.join(", "), b.join(", "))
            }
            CompareWarning::GitDirty { a, b } => write!(f, "git-dirty build in {a} A runs and {b} B runs"),
            CompareWarning::ShaMismatch { a, b } => write!(f, "sha_mismatch in {a} A runs and {b} B runs"),
            CompareWarning::OfferedLoadDiffers { field, a, b } => write!(
                f,
                "offered load differs: {field} (A: {}; B: {})",
                a.join(", "),
                b.join(", ")
            ),
        }
    }
}

impl CompareWarning {
    /// Whether the verdict already states this as an inconclusive reason.
    fn is_reason(&self) -> bool {
        matches!(
            self,
            CompareWarning::Ci {
                warning: CiWarning::EpisodeLimited { .. } | CiWarning::BetweenRunVarianceUnknown { .. },
                ..
            }
        )
    }
}

/// One line, for example "B slower at p99 by 401.0 ms [380.2, 452.9], 5 vs
/// 5 runs, nominal 95%, 2000 resamples, seed 0xA5EED0000000000A", followed
/// by any warning not already given as a reason. PerRunWelch ends "nominal
/// 95%, per-run Welch t, df 7.3" instead. A refusal computed no CI, so it
/// shows no level, method, resamples or seed.
impl fmt::Display for Comparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = stats::quantile_label(self.q);
        let runs = format!("{} vs {} runs", self.runs_a, self.runs_b);
        let level = match (self.method, self.df) {
            (CompareMethod::Hierarchical, _) => format!(
                "nominal {}%, {} resamples, seed {:#018X}",
                self.level * 100.0,
                self.resamples,
                self.seed
            ),
            (CompareMethod::PerRunWelch, Some(df)) => {
                format!("nominal {}%, per-run Welch t, df {df:.1}", self.level * 100.0)
            }
            (CompareMethod::PerRunWelch, None) => format!("nominal {}%, per-run Welch t", self.level * 100.0),
        };
        match &self.verdict {
            Verdict::Slower { delta, lo, hi } => {
                let d = decimals_for(*delta);
                write!(
                    f,
                    "B slower at {p} by {} ms [{}, {}], {runs}, {level}",
                    unsigned_ms(*delta, d),
                    unsigned_ms(*lo, d),
                    unsigned_ms(*hi, d)
                )?;
            }
            // Magnitudes: B is faster by -delta, with CI [-hi, -lo].
            Verdict::Faster { delta, lo, hi } => {
                let d = decimals_for(*delta);
                write!(
                    f,
                    "B faster at {p} by {} ms [{}, {}], {runs}, {level}",
                    unsigned_ms(-delta, d),
                    unsigned_ms(-hi, d),
                    unsigned_ms(-lo, d)
                )?;
            }
            Verdict::Inconclusive { reason } => write!(f, "inconclusive at {p}: {reason}, {runs}, {level}")?,
            Verdict::Refused { reason } => write!(f, "refused at {p}: {reason}, {runs}")?,
        }
        let shown: Vec<String> = self
            .warnings
            .iter()
            .filter(|w| !w.is_reason())
            .map(ToString::to_string)
            .collect();
        if !shown.is_empty() {
            write!(f, " [warnings: {}]", shown.join("; "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::Synth;

    const R: usize = 5;

    fn model() -> Synth {
        Synth::new(0.5, 0.5, 0.0)
    }

    /// R runs of `model` at `scale`; seeds start at `base`.
    fn runs(model: &Synth, m: &Manifest, base: u64, scale: f64) -> Vec<RecordedRun> {
        (0..R as u64).map(|i| model.run(m, base + i, scale)).collect()
    }

    fn load_warning(c: &Comparison, field: &str) -> Option<CompareWarning> {
        c.warnings
            .iter()
            .find(|w| matches!(w, CompareWarning::OfferedLoadDiffers { field: f, .. } if *f == field))
            .cloned()
    }

    #[test]
    fn different_offered_load_warns_with_both_values() {
        let model = model();
        let m = model.manifest();
        let a = runs(&model, &m, 0, 1.0);
        let mut b = runs(&model, &m, 100, 1.0);
        let same = compare(&a, &b, 0.5, DEFAULT_MIN_EFFECT);
        assert_eq!(load_warning(&same, "interval_ns"), None);
        assert_eq!(load_warning(&same, "count"), None);

        for run in &mut b {
            run.manifest.interval_ns = 5_000_000;
            run.manifest.count = 2000;
        }
        let c = compare(&a, &b, 0.5, DEFAULT_MIN_EFFECT);
        println!("{c}");
        assert_eq!(
            load_warning(&c, "interval_ns"),
            Some(CompareWarning::OfferedLoadDiffers {
                field: "interval_ns",
                a: vec!["10000000".to_owned()],
                b: vec!["5000000".to_owned()],
            })
        );
        assert!(load_warning(&c, "count").is_some());
        let line = c.to_string();
        assert!(
            line.contains("offered load differs: interval_ns (A: 10000000; B: 5000000)"),
            "{line}"
        );
        assert!(
            line.contains("offered load differs: count (A: 1000; B: 2000)"),
            "{line}"
        );
    }

    #[test]
    fn run_lengths_more_than_2x_apart_warn() {
        let model = model();
        let m = model.manifest();
        let mut a = runs(&model, &m, 0, 1.0);
        a[0].result = crate::synth::run_result(model.samples(0, 1.0)[..400].to_vec());
        let b = runs(&model, &m, 100, 1.0);
        let c = compare(&a, &b, 0.5, DEFAULT_MIN_EFFECT);
        println!("{c}");
        let w = CompareWarning::Ci {
            group: Group::A,
            warning: CiWarning::RunLengthsUneven {
                shortest: 400,
                longest: 1000,
            },
        };
        assert!(c.warnings.contains(&w), "{:?}", c.warnings);
        assert!(c.to_string().contains("pooling weights runs by length"));
    }

    #[test]
    fn comparison_records_seed_and_resamples() {
        let model = model();
        let m = model.manifest();
        let c = compare(
            &runs(&model, &m, 0, 1.0),
            &runs(&model, &m, 100, 1.0),
            0.5,
            DEFAULT_MIN_EFFECT,
        );
        println!("{c}");
        assert_eq!((c.seed, c.resamples), (DEFAULT_SEED, DEFAULT_RESAMPLES));
        assert!(c.to_string().contains("2000 resamples, seed 0xA5EED0000000000A"), "{c}");
    }

    /// B 5% slower: 0.5 ms at the 10 ms median. The CI resolves it, so with
    /// min_effect 0 the verdict is Slower; the default 1 ms min_effect
    /// calls it too small to tell from the timer.
    #[test]
    fn default_min_effect_hides_sub_millisecond_delta() {
        assert_eq!(DEFAULT_MIN_EFFECT, Duration::from_millis(1));
        let model = model();
        let m = model.manifest();
        let (a, b) = (runs(&model, &m, 0, 1.0), runs(&model, &m, 100, 1.05));
        let zero = compare(&a, &b, 0.5, Duration::ZERO);
        let default = compare(&a, &b, 0.5, DEFAULT_MIN_EFFECT);
        println!("{zero}\n{default}");
        assert!(matches!(zero.verdict, Verdict::Slower { .. }), "{zero}");
        let Verdict::Inconclusive { reason } = &default.verdict else {
            panic!("expected Inconclusive: {default}");
        };
        assert!(reason.contains("effect below min_effect: |delta| < 1.0 ms"), "{reason}");
    }

    /// PerRunWelch: delta is the mean of B's per-run p99 minus the mean of
    /// A's, and the CI is `stats::welch_ci` on those per-run values.
    #[test]
    fn per_run_welch_is_welch_on_per_run_quantiles() {
        let model = model();
        let m = model.manifest();
        let (a, b) = (runs(&model, &m, 0, 1.0), runs(&model, &m, 100, 1.3));
        let c = compare_with(&a, &b, 0.99, DEFAULT_MIN_EFFECT, CompareMethod::PerRunWelch);
        println!("{c}");
        let per_run = |runs: &[RecordedRun]| -> Vec<f64> {
            runs.iter()
                .map(|r| {
                    let lat: Vec<u64> = r.result.samples.iter().map(|s| s.latency_ns).collect();
                    stats::percentile(&lat, 0.99).unwrap() as f64
                })
                .collect()
        };
        let want = stats::welch_ci(&per_run(&a), &per_run(&b), DEFAULT_LEVEL).unwrap();
        assert_eq!(
            c.verdict,
            Verdict::Slower {
                delta: want.delta.round() as i64,
                lo: want.lo.round() as i64,
                hi: want.hi.round() as i64,
            }
        );
        assert_eq!((c.method, c.df), (CompareMethod::PerRunWelch, Some(want.df)));
        assert!(
            c.to_string().contains(&format!("per-run Welch t, df {:.1}", want.df)),
            "{c}"
        );
        assert!(!c.to_string().contains("resamples"), "{c}");
        assert_eq!(
            compare(&a, &b, 0.99, DEFAULT_MIN_EFFECT).method,
            CompareMethod::Hierarchical
        );
    }

    /// The refusals, the R < MIN_RUNS rule and the episode guard do not
    /// depend on the method.
    #[test]
    fn both_methods_share_refusals_and_guards() {
        let model = model();
        let m = model.manifest();
        let (a, b) = (runs(&model, &m, 0, 1.0), runs(&model, &m, 100, 1.0));

        let mut closed = runs(&model, &m, 200, 1.0);
        closed[0].manifest.runner = RunnerKind::ClosedLoop;

        // Every A run constant at 10 ms but one, which holds a single
        // 60-sample stall at 300 ms: the pooled p99 (rank 4950 of 5000) lies
        // in that one stall, so it is one episode.
        let flat = Synth::new(0.0, 0.0, 0.0);
        let mut stalled = runs(&flat, &m, 0, 1.0);
        let mut s = stalled[2].result.samples.clone();
        for x in &mut s[400..460] {
            x.latency_ns = 300_000_000;
            x.service_ns = Some(300_000_000);
        }
        stalled[2].result = crate::synth::run_result(s);

        for method in [CompareMethod::Hierarchical, CompareMethod::PerRunWelch] {
            let c = compare_with(&a[..2], &b[..2], 0.5, Duration::ZERO, method);
            println!("{c}");
            assert!(
                matches!(&c.verdict, Verdict::Inconclusive { reason } if reason.contains("R < 3 (A: 2, B: 2)")),
                "{method:?}: {c}"
            );
            let c = compare_with(&a, &closed, 0.5, Duration::ZERO, method);
            assert!(matches!(c.verdict, Verdict::Refused { .. }), "{method:?}: {c}");
            assert_eq!(c.df, None);
            let c = compare_with(&stalled, &b, 0.99, Duration::ZERO, method);
            println!("{c}");
            assert!(
                matches!(&c.verdict, Verdict::Inconclusive { reason } if reason.contains("episode-limited: 1 tail episodes")),
                "{method:?}: {c}"
            );
        }
        // One run per group: Welch has no variance, so no CI at all.
        let c = compare_with(&a[..1], &b[..1], 0.5, Duration::ZERO, CompareMethod::PerRunWelch);
        println!("{c}");
        assert!(c.to_string().contains("no CI of delta"), "{c}");
        assert!(c.to_string().contains(", no CI)"), "{c}");
    }
}
