//! Episodes: maximal runs of consecutive slow samples, and the body that is
//! left when they are taken out.
//!
//! This file exists so a stall and a slower normal path can be judged
//! separately. Once a stall covers more than 1% of a run, that run's p99
//! lands inside the stall, so per-run p99 cannot see a regression in the
//! rest of the run (docs/calibration.md). Splitting every run at one
//! threshold T, the same for A and B, gives a body to compare by percentile
//! and episodes to compare by how often they happen.
//!
//! The body is every request sent outside an episode window (`windows`):
//! not just the samples below T, because a freeze delays every request sent
//! during it until it ends, and the last ones wait less than T. Left in
//! the body, they made a stall-only change read as a slower body (1.4 ms at
//! p99 on the fake server of tests/compare.rs).
//!
//! The body is still biased, slightly:
//! - Low: a body sample at or above T that is not part of a stall (a
//!   natural tail excursion) is taken out with the stalls, so the body is
//!   the run's distribution cut at T. In tests/stall.rs the bias is 0. In
//!   the synthetic model of docs/calibration.md (lognormal, sigma 0.5,
//!   T = 10 x median, no stalls) the mean per-run body p99 is 0.00% to
//!   0.06% low ("Split verdict" section of the calibration report).
//! - High, with stalls: in that model the mean per-run body p99 is 0.2% to
//!   1.0% high. The body has fewer samples than the run, and the nearest
//!   rank ceil(0.99 n) of an n that is not a multiple of 100 lies a little
//!   higher in the distribution than rank 990 of 1000.

use std::fmt;
use std::time::Duration;

use crate::Sample;
use crate::stats;

/// Default `ThresholdRule::MedianFactor`: T = 10 x the pooled median.
///
/// 10, not 5: with T = 5 x median, the lognormal body of the calibration
/// model (sigma 0.5) crosses T on its own about 0.4 to 0.5 times per run,
/// and more often when the body is slower, so a body regression was
/// reported as "more episodes" in up to 41% of trials. At 10 x it crosses
/// almost never (see the calibration report's "Split verdict" section).
pub const DEFAULT_THRESHOLD_FACTOR: f64 = 10.0;

/// How the episode threshold T is set.
///
/// When the service has its own definition of a stall or an SLO latency
/// bound (say, "a request over 500 ms is an incident"), use it as an
/// `Absolute` threshold: then an episode means what the service's owners
/// mean by one, and T does not move when either group's median moves.
///
/// Whatever the rule, T only separates "slow" from "normal" if the body is
/// unimodal. If the body has a slow mode above T (a cache miss, a cold
/// path, a retry), every sample of that mode is counted as episodes, not
/// body: the body verdict cannot see a regression in that mode, and a
/// change in how often the mode happens is reported as more or fewer
/// episodes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThresholdRule {
    /// T = factor x the median of every A and B sample pooled, rounded to
    /// whole nanoseconds.
    MedianFactor(f64),
    /// T as given.
    Absolute(Duration),
}

impl Default for ThresholdRule {
    fn default() -> Self {
        ThresholdRule::MedianFactor(DEFAULT_THRESHOLD_FACTOR)
    }
}

/// The episode threshold T, with how it was derived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Threshold {
    /// A sample with `latency_ns >= ns` is in an episode.
    pub ns: u64,
    pub rule: ThresholdRule,
    /// Nearest-rank median of every sample of every run passed to `derive`,
    /// recorded whichever the rule.
    pub pooled_median_ns: u64,
}

impl Threshold {
    /// T for `runs` (A's and B's together) by `rule`. `None` if there are
    /// no samples.
    ///
    /// Panics if a `MedianFactor` is not finite and > 1, or an `Absolute`
    /// is zero.
    pub fn derive(runs: &[&[Sample]], rule: ThresholdRule) -> Option<Self> {
        let pooled: Vec<u64> = runs.iter().flat_map(|run| run.iter().map(|s| s.latency_ns)).collect();
        let pooled_median_ns = stats::percentile(&pooled, 0.5)?;
        let ns = match rule {
            ThresholdRule::MedianFactor(factor) => {
                assert!(
                    factor.is_finite() && factor > 1.0,
                    "threshold factor {factor} is not finite and > 1"
                );
                // At least 1 ns, so a sample of 0 ns is never an episode.
                ((factor * pooled_median_ns as f64).round() as u64).max(1)
            }
            ThresholdRule::Absolute(t) => {
                assert!(!t.is_zero(), "absolute threshold is zero");
                u64::try_from(t.as_nanos()).unwrap_or(u64::MAX)
            }
        };
        Some(Self {
            ns,
            rule,
            pooled_median_ns,
        })
    }
}

/// One run split at T.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunEpisodes {
    /// Maximal runs of consecutive samples, in intended send order, with
    /// latency >= T (`stats::tail_episodes`).
    pub episodes: usize,
    /// Samples in episodes: latency >= T.
    pub samples: usize,
    /// Samples intended while an episode was open (`windows`), the
    /// `samples` included. None of them is body.
    pub excluded: usize,
    /// Wall time during which an episode was open: the total length of
    /// `windows`.
    pub time_ns: u64,
    /// From the schedule start to the latest completion of any sample.
    pub span_ns: u64,
}

/// When an episode was open: the union, over episodes, of [intended send
/// of its first sample, latest completion (intended send + latency) of its
/// samples), as disjoint half-open intervals in order, in nanoseconds from
/// the schedule start.
///
/// A request sent inside a window is not body even if it took less than T.
/// A freeze delays every request sent during it until it ends, so the last
/// ones before the end wait less than T; left in the body, they would make
/// a stall look like a slower body.
pub fn windows(samples: &[Sample], threshold_ns: u64) -> Vec<(u64, u64)> {
    let done = |s: &Sample| s.intended_offset_ns.saturating_add(s.latency_ns);
    let mut out: Vec<(u64, u64)> = Vec::new();
    // Starts only grow, so a new episode either extends the last window or
    // starts after it.
    let mut push = |(start, end): (u64, u64)| match out.last_mut() {
        Some(last) if start <= last.1 => last.1 = last.1.max(end),
        _ => out.push((start, end)),
    };
    let mut open: Option<(u64, u64)> = None;
    for s in samples {
        if s.latency_ns >= threshold_ns {
            open = Some(match open {
                Some((start, end)) => (start, end.max(done(s))),
                None => (s.intended_offset_ns, done(s)),
            });
        } else if let Some(episode) = open.take() {
            push(episode);
        }
    }
    if let Some(episode) = open {
        push(episode);
    }
    out
}

/// Whether each sample was intended inside one of `windows`, in order.
fn in_windows<'a>(samples: &'a [Sample], windows: &'a [(u64, u64)]) -> impl Iterator<Item = (bool, &'a Sample)> {
    let mut w = 0;
    samples.iter().map(move |s| {
        let t = s.intended_offset_ns;
        while w < windows.len() && windows[w].1 <= t {
            w += 1;
        }
        (w < windows.len() && windows[w].0 <= t, s)
    })
}

/// `samples` split at `threshold_ns`.
pub fn run_episodes(samples: &[Sample], threshold_ns: u64) -> RunEpisodes {
    let latencies: Vec<u64> = samples.iter().map(|s| s.latency_ns).collect();
    let windows = windows(samples, threshold_ns);
    RunEpisodes {
        episodes: stats::tail_episodes(&latencies, threshold_ns),
        samples: latencies.iter().filter(|&&l| l >= threshold_ns).count(),
        excluded: in_windows(samples, &windows).filter(|(inside, _)| *inside).count(),
        time_ns: windows.iter().map(|(start, end)| end - start).sum(),
        span_ns: samples
            .iter()
            .map(|s| s.intended_offset_ns.saturating_add(s.latency_ns))
            .max()
            .unwrap_or(0),
    }
}

/// The body: the samples of `samples` intended outside every episode
/// window (`windows`), in their order. All of them are below
/// `threshold_ns`.
pub fn body(samples: &[Sample], threshold_ns: u64) -> Vec<Sample> {
    let windows = windows(samples, threshold_ns);
    in_windows(samples, &windows)
        .filter(|(inside, _)| !inside)
        .map(|(_, s)| *s)
        .collect()
}

/// "5.0 ms", or "0.500 ms" below 1 ms.
fn ms(ns: u64) -> String {
    let decimals = if ns >= 1_000_000 { 1 } else { 3 };
    format!("{:.*} ms", decimals, ns as f64 / 1e6)
}

/// ">= 100.0 ms = 10 x pooled median 10.0 ms", or ">= 50.0 ms, set by caller
/// (pooled median 10.0 ms)".
impl fmt::Display for Threshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (t, median) = (ms(self.ns), ms(self.pooled_median_ns));
        match self.rule {
            ThresholdRule::MedianFactor(factor) => write!(f, ">= {t} = {factor} x pooled median {median}"),
            ThresholdRule::Absolute(_) => write!(f, ">= {t}, set by caller (pooled median {median})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// Samples every 10 ms with the given latencies in ms.
    fn run(latencies_ms: &[u64]) -> Vec<Sample> {
        (0u64..)
            .zip(latencies_ms)
            .map(|(i, &l)| Sample {
                intended_offset_ns: i * 10 * MS,
                latency_ns: l * MS,
                service_ns: Some(l * MS),
                scheduler_lag_ns: Some(0),
            })
            .collect()
    }

    #[test]
    fn threshold_is_factor_times_pooled_median_of_both_groups() {
        // Pooled: 1, 1, 2, 3 | 4, 6 -> nearest-rank median (rank 3 of 6) = 2 ms.
        let (a, b) = (run(&[3, 1, 2, 1]), run(&[6, 4]));
        let t = Threshold::derive(&[&a, &b], ThresholdRule::default()).unwrap();
        assert_eq!((t.ns, t.pooled_median_ns), (20 * MS, 2 * MS));
        assert_eq!(t.to_string(), ">= 20.0 ms = 10 x pooled median 2.0 ms");

        let t = Threshold::derive(&[&a, &b], ThresholdRule::MedianFactor(2.5)).unwrap();
        assert_eq!(t.ns, 5 * MS);
        let t = Threshold::derive(&[&a, &b], ThresholdRule::Absolute(Duration::from_millis(7))).unwrap();
        assert_eq!((t.ns, t.pooled_median_ns), (7 * MS, 2 * MS));
        assert_eq!(t.to_string(), ">= 7.0 ms, set by caller (pooled median 2.0 ms)");
        assert_eq!(Threshold::derive(&[&[]], ThresholdRule::default()), None);
    }

    #[test]
    #[should_panic(expected = "factor")]
    fn threshold_factor_must_exceed_1() {
        Threshold::derive(&[&run(&[1])], ThresholdRule::MedianFactor(1.0));
    }

    /// Two episodes at T = 10 ms: samples 1-2 (one run of two) and sample 5.
    /// Sample 4 is 9 ms, below T, so it splits them.
    #[test]
    fn episodes_are_maximal_runs_at_or_above_threshold() {
        let s = run(&[1, 10, 40, 1, 9, 25, 1]);
        let e = run_episodes(&s, 10 * MS);
        assert_eq!((e.episodes, e.samples), (2, 3));
        let lat: Vec<u64> = s.iter().map(|s| s.latency_ns).collect();
        assert_eq!(e.episodes, stats::tail_episodes(&lat, 10 * MS));
        // Episode 1: [10, max(10 + 10, 20 + 40) = 60) ms; episode 2: [50, 75) ms.
        // They overlap, so the union is [10, 75) = 65 ms, not 50 + 25.
        assert_eq!(windows(&s, 10 * MS), [(10 * MS, 75 * MS)]);
        assert_eq!(e.time_ns, 65 * MS);
        // Latest completion is sample 5's, 50 + 25 = 75 ms, not the last sample's (61).
        assert_eq!(e.span_ns, 75 * MS);
        // Samples 3 and 6 (1 ms) and 4 (9 ms) are below T but were sent at
        // 30, 40 and 60 ms, inside the window: not body. Only sample 0 is.
        assert_eq!(e.excluded, 6);
        let body: Vec<u64> = body(&s, 10 * MS).iter().map(|s| s.latency_ns / MS).collect();
        assert_eq!(body, [1]);
    }

    /// A window is half-open: a request sent at the moment the episode's
    /// last request completes is body.
    #[test]
    fn window_ends_at_latest_completion_exclusive() {
        // Sample 1 (sent at 10 ms, 20 ms) completes at 30 ms; samples 2 and 3
        // are sent at 20 and 30 ms.
        let s = run(&[1, 20, 1, 1]);
        assert_eq!(windows(&s, 10 * MS), [(10 * MS, 30 * MS)]);
        let body: Vec<u64> = body(&s, 10 * MS).iter().map(|s| s.intended_offset_ns / MS).collect();
        assert_eq!(body, [0, 30]);
        assert_eq!(run_episodes(&s, 10 * MS).excluded, 2);
    }

    #[test]
    fn no_episodes_below_threshold() {
        let s = run(&[1, 2, 3]);
        assert_eq!(
            run_episodes(&s, 10 * MS),
            RunEpisodes {
                episodes: 0,
                samples: 0,
                excluded: 0,
                time_ns: 0,
                span_ns: 23 * MS,
            }
        );
        assert_eq!(body(&s, 10 * MS), s);
    }
}
