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

use crate::episodes::{self, RunEpisodes, Threshold, ThresholdRule};
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
///
/// PerRunWelch is the default because it holds its nominal false-verdict
/// rate across the settings in docs/calibration.md, and Hierarchical does
/// not once runs differ by 10% or more (its interval is too narrow for R of
/// 3 to 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompareMethod {
    /// delta = the percentile of B's runs pooled minus that of A's. The CI
    /// is the percentile interval of paired `stats::hier_ci` bootstrap
    /// draws (`DEFAULT_RESAMPLES`, `DEFAULT_SEED`). Runs weigh by length.
    Hierarchical,
    /// Each run's percentile is one observation, so each run weighs the
    /// same: delta = the mean of B's per-run percentiles minus the mean of
    /// A's, and the CI is Welch's t interval with Welch-Satterthwaite df
    /// (`stats::welch_ci`). No resampling: `Comparison::resamples` and
    /// `seed` do not apply.
    #[default]
    PerRunWelch,
}

impl CompareMethod {
    /// What delta is a difference of: "pooled" or "mean per-run".
    pub fn estimand(self) -> &'static str {
        match self {
            CompareMethod::Hierarchical => "pooled",
            CompareMethod::PerRunWelch => "mean per-run",
        }
    }
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
    /// True for the body of `compare_split`: samples in episodes were taken
    /// out of every run before the percentile was computed.
    pub episodes_excluded: bool,
    pub warnings: Vec<CompareWarning>,
}

/// Options for `compare_split`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitOptions {
    /// As in `compare`, for the body; usually `DEFAULT_MIN_EFFECT`.
    pub min_effect: Duration,
    pub threshold: ThresholdRule,
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self {
            min_effect: DEFAULT_MIN_EFFECT,
            threshold: ThresholdRule::default(),
        }
    }
}

/// Fewest episodes, A and B together, before the episode test may name a
/// direction.
pub const MIN_TEST_EPISODES: usize = 3;

/// What the episode counts support about B's episode rate in time against
/// A's.
#[derive(Debug, Clone, PartialEq)]
pub enum EpisodeOutcome {
    /// B has more episodes per unit of time: the test rejects equal rates.
    More,
    /// B has fewer episodes per unit of time: the test rejects equal rates.
    Fewer,
    /// Every rule other than p < 1 - level that failed; empty when only
    /// the p-value did.
    Inconclusive { reasons: Vec<String> },
    /// The runs cannot be compared (the reason `compare` gives).
    Refused { reason: String },
}

/// The episode part of `compare_split`.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeVerdict {
    pub outcome: EpisodeOutcome,
    /// Each run split at the threshold, in input order; empty when refused.
    pub a: Vec<RunEpisodes>,
    pub b: Vec<RunEpisodes>,
    /// Two-sided p-value of the exact conditional test; `None` when refused.
    pub p: Option<f64>,
    /// The test names a direction only when p < 1 - level.
    pub level: f64,
    /// Inconclusive with k >= `MIN_TEST_EPISODES` only: what this k could
    /// have shown.
    pub detectable: Option<Detectable>,
}

/// The least extreme splits of k episodes between A and B that the episode
/// test would call significant, as B's observed rate over A's (episodes
/// per unit of exposure). `more` is the smallest ratio above 1 that is
/// significant, `fewer` the largest below 1; infinity means only all k in
/// B, and 0 only all k in A; `None` means no split on that side is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detectable {
    pub k: usize,
    pub more: Option<f64>,
    pub fewer: Option<f64>,
    /// The test's level: significant means p < 1 - level.
    pub level: f64,
}

impl Detectable {
    /// By searching `stats::binomial_test` over k_A: its p-value only
    /// grows as k_A moves toward k x `share_a`, so each side is a binary
    /// search. `exposure_a` and `exposure_b` turn a split into a rate ratio.
    fn search(k: u64, share_a: f64, exposure_a: f64, exposure_b: f64, level: f64) -> Self {
        let significant = |ka: u64| stats::binomial_test(ka, k, share_a) < 1.0 - level;
        let ratio = |ka: u64| ((k - ka) as f64 / exposure_b) / (ka as f64 / exposure_a);
        let mean = k as f64 * share_a;
        // More: k_A below the mean, from 0 up to the last integer under it.
        let more = (mean > 0.0 && significant(0)).then(|| {
            let last = (mean.ceil() as u64).saturating_sub(1).min(k);
            ratio(last_true(0, last, significant))
        });
        // Fewer: k_A above the mean, from the first integer over it up to k.
        let fewer = (mean < k as f64 && significant(k)).then(|| {
            let first = ((mean.floor() as u64) + 1).min(k);
            ratio(first_true(first, k, significant))
        });
        Self {
            k: k as usize,
            more,
            fewer,
            level,
        }
    }
}

/// The largest x in [lo, hi] with `f(x)`, given `f(lo)` and that `f` is
/// true up to some point and false after it.
fn last_true(mut lo: u64, mut hi: u64, f: impl Fn(u64) -> bool) -> u64 {
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if f(mid) { lo = mid } else { hi = mid - 1 }
    }
    lo
}

/// The smallest x in [lo, hi] with `f(x)`, given `f(hi)` and that `f` is
/// false up to some point and true after it.
fn first_true(mut lo: u64, mut hi: u64, f: impl Fn(u64) -> bool) -> u64 {
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if f(mid) { hi = mid } else { lo = mid + 1 }
    }
    hi
}

/// "with 7 episodes only all 7 in B or all 7 in A is detectable at 95%",
/// "with 20 episodes only B/A >= 3.0x or B/A <= 0.33x is detectable at 95%".
impl fmt::Display for Detectable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let k = self.k;
        let more = self.more.map(|r| {
            if r.is_infinite() {
                format!("all {k} in B")
            } else {
                format!("B/A >= {r:.1}x")
            }
        });
        let fewer = self.fewer.map(|r| {
            if r == 0.0 {
                format!("all {k} in A")
            } else {
                format!("B/A <= {r:.2}x")
            }
        });
        let sides: Vec<String> = more.into_iter().chain(fewer).collect();
        let level = self.level * 100.0;
        if sides.is_empty() {
            write!(
                f,
                "with {k} episodes no split between A and B is significant at {level}%"
            )
        } else {
            write!(
                f,
                "with {k} episodes only {} is detectable at {level}%",
                sides.join(" or ")
            )
        }
    }
}

/// A two-part verdict: the body (requests sent outside episodes) by
/// percentile, and the episodes by how often they happen.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitComparison {
    pub body: Comparison,
    pub episodes: EpisodeVerdict,
    /// The threshold both groups were split at; `None` when refused.
    pub threshold: Option<Threshold>,
}

/// Compares the `q` percentile of latency between runs `a` and `b`, by the
/// default `CompareMethod` (PerRunWelch): `compare_with` with that method.
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
        None => decide(&samples(a), &samples(b), q, min_effect, method, true, &mut warnings),
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
        episodes_excluded: false,
        warnings,
    }
}

/// Compares A and B in two parts, so a regression in normal operation and a
/// change in how often the target stalls are judged separately.
///
/// Every run of A and B is split at one threshold T (`Threshold::derive`
/// on all runs, by `opts.threshold`): samples at or above T are episodes,
/// and the requests sent while no episode was open are the body (see
/// `episodes::windows`).
///
/// - body: PerRunWelch on each run's body samples, with the refusals,
///   warnings, `MIN_RUNS` rule and `opts.min_effect` of `compare_with`, but
///   no episode guard: the episodes were taken out. Refused if a run has no
///   body samples.
/// - episodes: an exact conditional test on the episode rate in time. The
///   exposure E of a group is the sum of its runs' `span_ns` (schedule
///   start to last completion), episode time included. Under equal rates
///   per unit of time, given k = k_A + k_B episodes in all,
///   k_A ~ Binomial(k, E_A / (E_A + E_B)); p is `stats::binomial_test`
///   (two-sided). This is exact if episodes arrive as a Poisson process
///   with the same rate in both groups. More or Fewer only if
///   p < 1 - `DEFAULT_LEVEL`, both groups have >= `MIN_RUNS` runs, and
///   k >= `MIN_TEST_EPISODES`; an inconclusive test with k >= 3 also says
///   what it could have detected (`Detectable`).
///
/// Both parts are refused, with no threshold, when `compare` would refuse.
pub fn compare_split(a: &[RecordedRun], b: &[RecordedRun], q: f64, opts: SplitOptions) -> SplitComparison {
    stats::check_quantile(q);
    let mut warnings = manifest_warnings(a, b);
    let method = CompareMethod::PerRunWelch;
    let body = |verdict, df, warnings| Comparison {
        verdict,
        q,
        runs_a: a.len(),
        runs_b: b.len(),
        level: DEFAULT_LEVEL,
        method,
        resamples: DEFAULT_RESAMPLES,
        seed: DEFAULT_SEED,
        df,
        episodes_excluded: true,
        warnings,
    };
    if let Some(reason) = refusal(a, b) {
        return SplitComparison {
            body: body(Verdict::Refused { reason: reason.clone() }, None, warnings),
            episodes: EpisodeVerdict {
                outcome: EpisodeOutcome::Refused { reason },
                a: Vec::new(),
                b: Vec::new(),
                p: None,
                level: DEFAULT_LEVEL,
                detectable: None,
            },
            threshold: None,
        };
    }

    let (sa, sb) = (samples(a), samples(b));
    let threshold = Threshold::derive(&[sa.as_slice(), sb.as_slice()].concat(), opts.threshold)
        .expect("refusal rules out empty runs");
    let split =
        |runs: &[&[Sample]]| -> Vec<Vec<Sample>> { runs.iter().map(|r| episodes::body(r, threshold.ns)).collect() };
    let (body_a, body_b) = (split(&sa), split(&sb));
    let empty = |runs: &[Vec<Sample>]| runs.iter().filter(|r| r.is_empty()).count();
    let (empty_a, empty_b) = (empty(&body_a), empty(&body_b));
    let (verdict, df) = if empty_a + empty_b > 0 {
        let reason = format!("{empty_a} A runs and {empty_b} B runs have no samples outside episodes, so no body");
        (Verdict::Refused { reason }, None)
    } else {
        let (body_a, body_b): (Vec<&[Sample]>, Vec<&[Sample]>) = (
            body_a.iter().map(Vec::as_slice).collect(),
            body_b.iter().map(Vec::as_slice).collect(),
        );
        decide(&body_a, &body_b, q, opts.min_effect, method, false, &mut warnings)
    };

    let per_run = |runs: &[&[Sample]]| -> Vec<RunEpisodes> {
        runs.iter().map(|r| episodes::run_episodes(r, threshold.ns)).collect()
    };
    SplitComparison {
        body: body(verdict, df, warnings),
        episodes: episode_verdict(per_run(&sa), per_run(&sb)),
        threshold: Some(threshold),
    }
}

/// The exact conditional test on per-run episode counts (see
/// `compare_split`).
fn episode_verdict(a: Vec<RunEpisodes>, b: Vec<RunEpisodes>) -> EpisodeVerdict {
    let count = |runs: &[RunEpisodes]| runs.iter().map(|r| r.episodes).sum::<usize>();
    let exposure = |runs: &[RunEpisodes]| runs.iter().map(|r| u128::from(r.span_ns)).sum::<u128>();
    let (ka, kb) = (count(&a), count(&b));
    let (ra, rb) = (a.len(), b.len());
    // Exposure is 0 only if every sample was intended at 0 and took 0 ns;
    // then fall back to counting runs.
    let (ea, eb) = match (exposure(&a), exposure(&b)) {
        (0, 0) => (ra as u128, rb as u128),
        e => e,
    };
    let share_a = ea as f64 / (ea + eb) as f64;
    let p = stats::binomial_test(ka as u64, (ka + kb) as u64, share_a);
    let mut reasons = Vec::new();
    if ra < MIN_RUNS || rb < MIN_RUNS {
        reasons.push(format!("R < {MIN_RUNS} (A: {ra}, B: {rb})"));
    }
    if ka + kb < MIN_TEST_EPISODES {
        reasons.push(format!("too few episodes ({}, want >= {MIN_TEST_EPISODES})", ka + kb));
    }
    let alpha = 1.0 - DEFAULT_LEVEL;
    let outcome = if reasons.is_empty() && p < alpha {
        // kb / eb against ka / ea, in integers.
        if kb as u128 * ea > ka as u128 * eb {
            EpisodeOutcome::More
        } else {
            EpisodeOutcome::Fewer
        }
    } else {
        EpisodeOutcome::Inconclusive { reasons }
    };
    let detectable = (matches!(outcome, EpisodeOutcome::Inconclusive { .. }) && ka + kb >= MIN_TEST_EPISODES)
        .then(|| Detectable::search((ka + kb) as u64, share_a, ea as f64, eb as f64, DEFAULT_LEVEL));
    EpisodeVerdict {
        outcome,
        a,
        b,
        p: Some(p),
        level: DEFAULT_LEVEL,
        detectable,
    }
}

impl EpisodeVerdict {
    /// Episodes, summed over the group's runs.
    pub fn episodes(&self, group: Group) -> usize {
        self.runs(group).iter().map(|r| r.episodes).sum()
    }

    /// The group's exposure: its runs' `span_ns`, summed.
    pub fn exposure_ns(&self, group: Group) -> u64 {
        self.runs(group).iter().map(|r| r.span_ns).sum()
    }

    /// Episodes per minute of exposure; 0 for no exposure.
    pub fn rate_per_min(&self, group: Group) -> f64 {
        match self.exposure_ns(group) {
            0 => 0.0,
            e => self.episodes(group) as f64 / (e as f64 / 60e9),
        }
    }

    /// Fraction of the group's wall time with an episode open: summed
    /// `time_ns` over summed `span_ns`; 0 for no runs.
    pub fn time_fraction(&self, group: Group) -> f64 {
        let runs = self.runs(group);
        let span: u64 = runs.iter().map(|r| r.span_ns).sum();
        if span == 0 {
            return 0.0;
        }
        runs.iter().map(|r| r.time_ns).sum::<u64>() as f64 / span as f64
    }

    fn runs(&self, group: Group) -> &[RunEpisodes] {
        match group {
            Group::A => &self.a,
            Group::B => &self.b,
        }
    }
}

/// "1.00", "0.031", or "2.0e-4" below 0.001.
fn p_value(p: f64) -> String {
    if p >= 0.1 {
        format!("{p:.2}")
    } else if p >= 0.001 {
        format!("{p:.3}")
    } else {
        format!("{p:.1e}")
    }
}

/// Two lines, for example
/// "body: B slower at p99 (mean per-run, episodes excluded) by 6.2 ms [3.1, 9.3], 10 vs 10 runs, Welch t df 17.2, nominal 95%"
/// "episodes (>= 100.0 ms = 10 x pooled median 10.0 ms): A 2 in 10 runs, 100.1 s (1.20/min, 0.95% of time), B 3 in 10
/// runs, 100.2 s (1.80/min, 1.43% of time); inconclusive, p = 1.00; with 5 episodes no split between A and B is
/// significant at 95%".
impl fmt::Display for SplitComparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "body: {}", self.body)?;
        let e = &self.episodes;
        let (Some(threshold), Some(p)) = (&self.threshold, e.p) else {
            let reason = match &e.outcome {
                EpisodeOutcome::Refused { reason } => reason.as_str(),
                _ => "no threshold",
            };
            return write!(
                f,
                "episodes: refused: {reason}, {} vs {} runs",
                self.body.runs_a, self.body.runs_b
            );
        };
        let side = |group| {
            format!(
                "{} {} in {} runs, {:.1} s ({:.2}/min, {:.2}% of time)",
                group_label(group),
                e.episodes(group),
                e.runs(group).len(),
                e.exposure_ns(group) as f64 / 1e9,
                e.rate_per_min(group),
                e.time_fraction(group) * 100.0
            )
        };
        let outcome = match &e.outcome {
            EpisodeOutcome::More => "B more episodes per minute".to_owned(),
            EpisodeOutcome::Fewer => "B fewer episodes per minute".to_owned(),
            EpisodeOutcome::Inconclusive { reasons } if reasons.is_empty() => "inconclusive".to_owned(),
            EpisodeOutcome::Inconclusive { reasons } => format!("inconclusive: {}", reasons.join("; ")),
            EpisodeOutcome::Refused { reason } => format!("refused: {reason}"),
        };
        write!(
            f,
            "episodes ({threshold}): {}, {}; {outcome}, p = {}",
            side(Group::A),
            side(Group::B),
            p_value(p)
        )?;
        match &e.detectable {
            Some(d) => write!(f, "; {d}"),
            None => Ok(()),
        }
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
/// PerRunWelch; adds each group's CI warnings to `warnings`. Without
/// `episode_guard`, an episode-limited estimate is neither a reason nor a
/// warning (the body of `compare_split`).
fn decide(
    a: &[&[Sample]],
    b: &[&[Sample]],
    q: f64,
    min_effect: Duration,
    method: CompareMethod,
    episode_guard: bool,
    warnings: &mut Vec<CompareWarning>,
) -> (Verdict, Option<f64>) {
    let pooled = |runs| Pooled::new(runs, q).expect("refusal rules out empty groups and runs");
    let (pooled_a, pooled_b) = (pooled(a), pooled(b));
    for (group, p) in [(Group::A, &pooled_a), (Group::B, &pooled_b)] {
        warnings.extend(
            p.warnings
                .iter()
                .filter(|w| episode_guard || !matches!(w, CiWarning::EpisodeLimited { .. }))
                .map(|w| CompareWarning::Ci {
                    group,
                    warning: w.clone(),
                }),
        );
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
    if episode_guard && (pooled_a.episodes < MIN_EPISODES || pooled_b.episodes < MIN_EPISODES) {
        reasons.push(format!(
            "episode-limited: {} tail episodes at or above A's pooled {p} ({} ms) and {} at or above B's ({} ms), want >= \
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

/// One line that names the estimand, for example "B slower at p99 (mean
/// per-run) by 5.9 ms [2.1, 9.7], 5 vs 5 runs, Welch t df 7.4, nominal 95%"
/// or "B slower at p99 (pooled) by 401.0 ms [380.2, 452.9], 5 vs 5 runs,
/// hierarchical bootstrap, 2000 resamples, seed 0xA5EED0000000000A, nominal
/// 95%", followed by any warning not already given as a reason. A refusal
/// computed no estimate or CI, so it shows no estimand, method or level.
impl fmt::Display for Comparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = stats::quantile_label(self.q);
        let runs = format!("{} vs {} runs", self.runs_a, self.runs_b);
        let method = match (self.method, self.df) {
            (CompareMethod::Hierarchical, _) => format!(
                "hierarchical bootstrap, {} resamples, seed {:#018X}",
                self.resamples, self.seed
            ),
            (CompareMethod::PerRunWelch, Some(df)) => format!("Welch t df {df:.1}"),
            (CompareMethod::PerRunWelch, None) => "Welch t, no CI".to_owned(),
        };
        let level = format!("{method}, nominal {}%", self.level * 100.0);
        let at = if self.episodes_excluded {
            format!("{p} ({}, episodes excluded)", self.method.estimand())
        } else {
            format!("{p} ({})", self.method.estimand())
        };
        match &self.verdict {
            Verdict::Slower { delta, lo, hi } => {
                let d = decimals_for(*delta);
                write!(
                    f,
                    "B slower at {at} by {} ms [{}, {}], {runs}, {level}",
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
                    "B faster at {at} by {} ms [{}, {}], {runs}, {level}",
                    unsigned_ms(-delta, d),
                    unsigned_ms(-hi, d),
                    unsigned_ms(-lo, d)
                )?;
            }
            Verdict::Inconclusive { reason } => write!(f, "inconclusive at {at}: {reason}, {runs}, {level}")?,
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

    /// Hierarchical names its estimand "pooled" and records its bootstrap.
    #[test]
    fn comparison_records_seed_and_resamples() {
        let model = model();
        let m = model.manifest();
        let c = compare_with(
            &runs(&model, &m, 0, 1.0),
            &runs(&model, &m, 100, 1.0),
            0.5,
            DEFAULT_MIN_EFFECT,
            CompareMethod::Hierarchical,
        );
        println!("{c}");
        assert_eq!((c.seed, c.resamples), (DEFAULT_SEED, DEFAULT_RESAMPLES));
        let line = c.to_string();
        assert!(line.starts_with("inconclusive at p50 (pooled): "), "{c}");
        assert!(
            line.contains("hierarchical bootstrap, 2000 resamples, seed 0xA5EED0000000000A, nominal 95%"),
            "{c}"
        );
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
        let line = c.to_string();
        assert!(line.starts_with("B slower at p99 (mean per-run) by "), "{c}");
        assert!(
            line.ends_with(&format!(", 5 vs 5 runs, Welch t df {:.1}, nominal 95%", want.df)),
            "{c}"
        );
        assert!(!c.to_string().contains("resamples"), "{c}");
        // PerRunWelch is the default: `compare` gives the same comparison.
        assert_eq!(compare(&a, &b, 0.99, DEFAULT_MIN_EFFECT), c);
        assert_eq!(CompareMethod::default(), CompareMethod::PerRunWelch);
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

    /// Synthetic runs with a 300 ms stall over 2% of every B run (prob 1)
    /// and none in A: the episode verdict is the exact test on the counts,
    /// weighted by exposure in time, and the body is Welch on what is left.
    #[test]
    fn split_episode_verdict_is_exact_test_on_counts() {
        let plain = Synth::new(0.5, 0.5, 0.0);
        let mut stalled = plain;
        stalled.stall = Some(crate::synth::Stall {
            prob: 1.0,
            extra_ns: 300e6,
            fraction: 0.02,
        });
        let m = plain.manifest();
        let a: Vec<RecordedRun> = (0..10).map(|i| plain.run(&m, i, 1.0)).collect();
        let b: Vec<RecordedRun> = (0..10).map(|i| stalled.run(&m, 100 + i, 1.0)).collect();
        let s = compare_split(&a, &b, 0.99, SplitOptions::default());
        println!("{s}");

        let t = s.threshold.expect("not refused");
        let all: Vec<&[Sample]> = samples(&a).into_iter().chain(samples(&b)).collect();
        assert_eq!(Some(t), Threshold::derive(&all, ThresholdRule::default()));
        let count = |runs: &[RunEpisodes]| runs.iter().map(|r| r.episodes).sum::<usize>() as u64;
        let (ka, kb) = (count(&s.episodes.a), count(&s.episodes.b));
        assert!(kb >= 10, "every B run has its stall: {s}");
        let (ea, eb) = (s.episodes.exposure_ns(Group::A), s.episodes.exposure_ns(Group::B));
        let share_a = ea as f64 / (ea + eb) as f64;
        assert_eq!(s.episodes.p, Some(stats::binomial_test(ka, ka + kb, share_a)));
        assert_eq!(s.episodes.outcome, EpisodeOutcome::More, "{s}");

        assert!(s.body.episodes_excluded && s.body.method == CompareMethod::PerRunWelch);
        let lines: Vec<String> = s.to_string().lines().map(str::to_owned).collect();
        assert_eq!(lines.len(), 2, "{s}");
        assert!(
            lines[0].starts_with("body: inconclusive at p99 (mean per-run, episodes excluded): "),
            "{s}"
        );
        assert!(
            lines[0].contains(", 10 vs 10 runs, Welch t df ") && lines[0].ends_with(", nominal 95%"),
            "{s}"
        );
        assert!(
            lines[1].starts_with(&format!(
                "episodes ({t}): A {ka} in 10 runs, {:.1} s (",
                ea as f64 / 1e9
            )),
            "{s}"
        );
        assert!(
            lines[1].contains(&format!("B {kb} in 10 runs, {:.1} s (", eb as f64 / 1e9)),
            "{s}"
        );
        assert!(lines[1].contains("; B more episodes per minute, p = "), "{s}");
        assert_eq!(s.episodes.detectable, None, "only for an inconclusive test");
        // The body is not held to the episode guard.
        assert!(
            !s.body.warnings.iter().any(|w| matches!(
                w,
                CompareWarning::Ci {
                    warning: CiWarning::EpisodeLimited { .. },
                    ..
                }
            )),
            "{:?}",
            s.body.warnings
        );
    }

    #[test]
    fn split_records_absolute_threshold_and_needs_runs_and_episodes() {
        let model = model();
        let m = model.manifest();
        let (a, b) = (runs(&model, &m, 0, 1.0), runs(&model, &m, 100, 1.0));
        let opts = SplitOptions {
            min_effect: Duration::ZERO,
            threshold: ThresholdRule::Absolute(Duration::from_secs(1)),
        };
        let s = compare_split(&a[..2], &b[..2], 0.99, opts);
        println!("{s}");
        let t = s.threshold.unwrap();
        assert_eq!((t.ns, t.rule), (1_000_000_000, opts.threshold));
        assert_eq!(
            s.episodes.outcome,
            EpisodeOutcome::Inconclusive {
                reasons: vec![
                    "R < 3 (A: 2, B: 2)".to_owned(),
                    "too few episodes (0, want >= 3)".to_owned()
                ]
            }
        );
        assert_eq!(s.episodes.p, Some(1.0));
        assert!(
            s.to_string()
                .contains("episodes (>= 1000.0 ms, set by caller (pooled median "),
            "{s}"
        );
        assert!(
            s.to_string().contains(
                "(0.00/min, 0.00% of time); inconclusive: R < 3 (A: 2, B: 2); too few episodes (0, want >= 3), p = 1.00"
            ),
            "{s}"
        );
        assert_eq!(s.episodes.detectable, None, "k < 3");
    }

    fn run_episodes_of(episodes: usize, span_s: u64) -> RunEpisodes {
        RunEpisodes {
            episodes,
            samples: episodes,
            excluded: episodes,
            time_ns: 0,
            span_ns: span_s * 1_000_000_000,
        }
    }

    /// The same count per run, but B's runs are half as long in time: B's
    /// rate per minute is twice A's, and the test weighs by exposure, not
    /// by runs.
    #[test]
    fn episode_test_weighs_by_exposure_in_time() {
        let a = vec![run_episodes_of(2, 20); 5];
        let b = vec![run_episodes_of(2, 10); 5];
        let e = episode_verdict(a, b);
        assert_eq!(
            (e.exposure_ns(Group::A), e.exposure_ns(Group::B)),
            (100_000_000_000, 50_000_000_000)
        );
        assert_eq!((e.rate_per_min(Group::A), e.rate_per_min(Group::B)), (6.0, 12.0));
        assert_eq!(e.p, Some(stats::binomial_test(10, 20, 2.0 / 3.0)));
        assert_ne!(e.p, Some(stats::binomial_test(10, 20, 0.5)), "not by runs");

        // 6 in 100 s against 24 in 50 s: B's rate is 8x A's.
        let e = episode_verdict(
            vec![run_episodes_of(2, 20), run_episodes_of(2, 40), run_episodes_of(2, 40)],
            vec![run_episodes_of(8, 10), run_episodes_of(8, 20), run_episodes_of(8, 20)],
        );
        assert_eq!(e.outcome, EpisodeOutcome::More, "{e:?}");
        assert_eq!(e.p, Some(stats::binomial_test(6, 30, 2.0 / 3.0)));
    }

    /// Every k_A by brute force: the least extreme significant split on
    /// each side, as B's rate over A's.
    fn detectable_by_scan(k: u64, share_a: f64, ea: f64, eb: f64) -> (Option<f64>, Option<f64>) {
        let mean = k as f64 * share_a;
        let ratio = |ka: u64| ((k - ka) as f64 / eb) / (ka as f64 / ea);
        let sig: Vec<u64> = (0..=k)
            .filter(|&ka| stats::binomial_test(ka, k, share_a) < 0.05)
            .collect();
        let more = sig.iter().filter(|&&ka| (ka as f64) < mean).max().map(|&ka| ratio(ka));
        let fewer = sig.iter().filter(|&&ka| (ka as f64) > mean).min().map(|&ka| ratio(ka));
        (more, fewer)
    }

    #[test]
    fn detectable_ratio_is_least_extreme_significant_split() {
        // k = 20, equal exposure: 5 vs 15 has p = 0.041, 6 vs 14 has 0.115.
        let d = Detectable::search(20, 0.5, 1.0, 1.0, DEFAULT_LEVEL);
        assert_eq!((d.more, d.fewer), (Some(3.0), Some(1.0 / 3.0)));
        assert_eq!(
            d.to_string(),
            "with 20 episodes only B/A >= 3.0x or B/A <= 0.33x is detectable at 95%"
        );
        // k = 7: only 0 vs 7 (p = 0.016); 1 vs 6 has p = 0.125.
        let d = Detectable::search(7, 0.5, 1.0, 1.0, DEFAULT_LEVEL);
        assert_eq!((d.more, d.fewer), (Some(f64::INFINITY), Some(0.0)));
        assert_eq!(
            d.to_string(),
            "with 7 episodes only all 7 in B or all 7 in A is detectable at 95%"
        );
        // k = 5: even 0 vs 5 has p = 0.0625.
        let d = Detectable::search(5, 0.5, 1.0, 1.0, DEFAULT_LEVEL);
        assert_eq!((d.more, d.fewer), (None, None));
        assert_eq!(
            d.to_string(),
            "with 5 episodes no split between A and B is significant at 95%"
        );

        for k in 3..80 {
            for (ea, eb) in [(1.0, 1.0), (3.0, 7.0), (7.0, 3.0), (1.0, 4.0)] {
                let share = ea / (ea + eb);
                let d = Detectable::search(k, share, ea, eb, DEFAULT_LEVEL);
                assert_eq!(
                    (d.more, d.fewer),
                    detectable_by_scan(k, share, ea, eb),
                    "k {k}, {ea} vs {eb}"
                );
            }
        }
    }

    /// Poisson stalls in time (several per run possible), bodies equal.
    #[test]
    fn split_poisson_stalls_rate_change_is_more_episodes() {
        let base = Synth::new(0.5, 0.5, 0.0);
        let with_rate = |per_s| {
            let mut m = base;
            m.poisson_stalls = Some(crate::synth::PoissonStalls {
                per_s,
                extra_ns: 300e6,
                fraction: 0.02,
            });
            m
        };
        let m = base.manifest();
        let (low, high) = (with_rate(0.2), with_rate(0.6));
        let a: Vec<RecordedRun> = (0..10).map(|i| low.run(&m, i, 1.0)).collect();
        let same: Vec<RecordedRun> = (0..10).map(|i| low.run(&m, 100 + i, 1.0)).collect();
        let more: Vec<RecordedRun> = (0..10).map(|i| high.run(&m, 200 + i, 1.0)).collect();

        let s = compare_split(&a, &more, 0.99, SplitOptions::default());
        println!("{s}");
        assert_eq!(s.episodes.outcome, EpisodeOutcome::More, "{s}");
        assert!(
            !matches!(s.body.verdict, Verdict::Slower { .. } | Verdict::Faster { .. }),
            "{s}"
        );

        let s = compare_split(&a, &same, 0.99, SplitOptions::default());
        println!("{s}");
        assert!(matches!(s.episodes.outcome, EpisodeOutcome::Inconclusive { .. }), "{s}");
        let d = s.episodes.detectable.expect("k >= 3");
        assert!(s.to_string().ends_with(&format!("; {d}")), "{s}");
    }

    /// A run made only of episodes leaves no body to take a percentile of.
    #[test]
    fn split_refuses_body_when_a_run_is_all_episodes() {
        let model = model();
        let m = model.manifest();
        let (a, b) = (runs(&model, &m, 0, 1.0), runs(&model, &m, 100, 1.0));
        let opts = SplitOptions {
            min_effect: Duration::ZERO,
            threshold: ThresholdRule::Absolute(Duration::from_nanos(1)),
        };
        let s = compare_split(&a, &b, 0.99, opts);
        println!("{s}");
        assert!(
            matches!(&s.body.verdict, Verdict::Refused { reason } if reason.contains("5 A runs and 5 B runs have no samples outside episodes")),
            "{s}"
        );
        assert!(s.threshold.is_some());
    }

    #[test]
    fn split_same_runs_has_no_direction() {
        let model = model();
        let m = model.manifest();
        let a = runs(&model, &m, 0, 1.0);
        let b = runs(&model, &m, 100, 1.0);
        let s = compare_split(&a, &b, 0.99, SplitOptions::default());
        println!("{s}");
        assert!(matches!(s.body.verdict, Verdict::Inconclusive { .. }), "{s}");
        assert!(matches!(s.episodes.outcome, EpisodeOutcome::Inconclusive { .. }), "{s}");
    }
}
