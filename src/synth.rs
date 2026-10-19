//! Synthetic runs for tests: `Vec<Sample>` built directly from the crate's
//! PRNG, with no runner and no clock. Unlike the paused-clock fake servers
//! in tests/, latencies have nanosecond resolution, and a trial costs only
//! what `compare` costs, so calibration can run thousands of trials.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::compare::RecordedRun;
use crate::manifest::{Manifest, Probe};
use crate::stats::SplitMix64;
use crate::{CountedHistogram, MIN_LAG_US, MIN_LATENCY_US, RunConfig, RunResult, RunnerKind, Sample};

/// Latency model, for run `seed` and a scale factor `scale`:
///
/// latency_i = scale * (median_ns * exp(sigma * x_i) + offset)
///
/// - x_i is a stationary AR(1) series,
///   x_i = phi * x_{i-1} + sqrt(1 - phi^2) * e_i with e_i ~ N(0, 1) and
///   x_0 ~ N(0, 1): every x_i is N(0, 1), and neighbours correlate (lag-k
///   correlation phi^k).
/// - offset ~ N(0, run_sd_ns^2), drawn once per run before x_0: every
///   sample of the run is shifted by it (between-run variance).
/// - Rounded to whole nanoseconds; a negative value saturates at 0.
/// - With `stall` set, a stalled run also adds `extra_ns` to each of a
///   contiguous `fraction` of its samples (see `Stall`).
///
/// Sample i is intended at i * interval; service = latency and scheduler
/// lag = 0, as on a perfect timer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Synth {
    pub median_ns: f64,
    pub sigma: f64,
    pub phi: f64,
    pub run_sd_ns: f64,
    pub stall: Option<Stall>,
    /// Samples per run.
    pub n: usize,
    pub interval: Duration,
}

/// A run stalls with probability `prob`; a stall adds `extra_ns` to every
/// sample of a contiguous `round(fraction * n)` samples starting at a
/// uniform position. The stall is the machine, not the code under test, so
/// `scale` does not apply to it. Whether and where a run stalls is drawn
/// from a stream of its own, so adding a stall leaves the body unchanged.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stall {
    pub prob: f64,
    pub extra_ns: f64,
    pub fraction: f64,
}

impl Stall {
    /// XORed into a run's seed to seed its stall stream.
    const STREAM: u64 = 0x57A1_1000_0000_0000;
}

impl Synth {
    /// Median of the body: 10 ms.
    pub const MEDIAN_NS: f64 = 10e6;

    /// 10 ms median, 1000 samples per run at 10 ms intervals.
    pub fn new(sigma: f64, phi: f64, run_sd_ns: f64) -> Self {
        Self {
            median_ns: Self::MEDIAN_NS,
            sigma,
            phi,
            run_sd_ns,
            stall: None,
            n: 1000,
            interval: Duration::from_millis(10),
        }
    }

    pub fn samples(&self, seed: u64, scale: f64) -> Vec<Sample> {
        let mut rng = SplitMix64::new(seed);
        let offset = self.run_sd_ns * rng.normal();
        let innovation = (1.0 - self.phi * self.phi).sqrt();
        let interval_ns = u64::try_from(self.interval.as_nanos()).expect("interval fits in u64 ns");
        let stalled = self.stall.and_then(|stall| {
            let mut rng = SplitMix64::new(seed ^ Stall::STREAM);
            let len = ((stall.fraction * self.n as f64).round() as usize).min(self.n);
            let start = (rng.next_f64() * (self.n - len + 1) as f64) as usize;
            (rng.next_f64() < stall.prob).then_some((start..start + len, stall.extra_ns))
        });
        let mut x = rng.normal();
        (0u64..)
            .take(self.n)
            .map(|i| {
                let body = self.median_ns * (self.sigma * x).exp();
                x = self.phi * x + innovation * rng.normal();
                let extra = match &stalled {
                    Some((range, extra_ns)) if range.contains(&(i as usize)) => *extra_ns,
                    _ => 0.0,
                };
                // f64 -> u64 `as` saturates: negative values become 0.
                let ns = (scale * (body + offset) + extra).round() as u64;
                Sample {
                    intended_offset_ns: i * interval_ns,
                    latency_ns: ns,
                    service_ns: Some(ns),
                    scheduler_lag_ns: Some(0),
                }
            })
            .collect()
    }

    /// An open-loop manifest for this machine and model, with the git fields
    /// set clean so results do not depend on the working tree. Blocking (runs
    /// git): collect once, then clone per run.
    pub fn manifest(&self) -> Manifest {
        let mut m = Manifest::collect(&self.config());
        m.build_git_dirty = Probe::Known(false);
        m.runtime_git_dirty = Probe::Known(false);
        m.sha_mismatch = Probe::Known(false);
        m
    }

    /// The open-loop config a real run of this model would have.
    pub fn config(&self) -> RunConfig {
        RunConfig {
            kind: RunnerKind::OpenLoop,
            interval: self.interval,
            count: self.n as u64,
            seed: None,
            extra: BTreeMap::from([("target".to_owned(), "synth".to_owned())]),
        }
    }

    /// `samples(seed, scale)` as an open-loop run, histograms included.
    pub fn run(&self, manifest: &Manifest, seed: u64, scale: f64) -> RecordedRun {
        let mut manifest = manifest.clone();
        manifest.seed = Some(seed);
        RecordedRun {
            result: run_result(self.samples(seed, scale)),
            manifest,
        }
    }
}

/// The `RunResult` `open_loop` would return for these samples.
pub(crate) fn run_result(samples: Vec<Sample>) -> RunResult {
    let mut latency = CountedHistogram::new(MIN_LATENCY_US);
    let mut service = CountedHistogram::new(MIN_LATENCY_US);
    let mut lag = CountedHistogram::new(MIN_LAG_US);
    for s in &samples {
        latency.record(Duration::from_nanos(s.latency_ns));
        service.record(Duration::from_nanos(s.service_ns.unwrap_or(s.latency_ns)));
        lag.record(Duration::from_nanos(s.scheduler_lag_ns.unwrap_or(0)));
    }
    RunResult {
        latency,
        service: Some(service),
        scheduler_lag: Some(lag),
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::percentile;

    fn latencies(s: &[Sample]) -> Vec<u64> {
        s.iter().map(|s| s.latency_ns).collect()
    }

    #[test]
    fn same_seed_same_samples_different_seed_different() {
        let m = Synth::new(0.5, 0.5, 3e5);
        assert_eq!(m.samples(1, 1.0), m.samples(1, 1.0));
        assert_ne!(m.samples(1, 1.0), m.samples(2, 1.0));
        let s = m.samples(1, 1.0);
        assert_eq!(s.len(), 1000);
        assert_eq!(s[3].intended_offset_ns, 30_000_000);
    }

    /// Nanosecond resolution: almost no value is a whole microsecond.
    #[test]
    fn latencies_have_nanosecond_resolution() {
        let s = latencies(&Synth::new(0.5, 0.5, 0.0).samples(3, 1.0));
        let whole_us = s.iter().filter(|&&ns| ns % 1000 == 0).count();
        assert!(whole_us < 10, "{whole_us} of {} are whole us", s.len());
    }

    /// With no offset, ln(latency / median) / sigma recovers x: its median is
    /// about 0 and its lag-1 correlation about phi.
    #[test]
    fn body_is_lognormal_ar1() {
        let mut m = Synth::new(0.5, 0.7, 0.0);
        m.n = 200_000;
        let s = latencies(&m.samples(5, 1.0));
        let median = percentile(&s, 0.5).unwrap() as f64;
        assert!((median / Synth::MEDIAN_NS - 1.0).abs() < 0.02, "median {median}");
        let x: Vec<f64> = s
            .iter()
            .map(|&ns| (ns as f64 / Synth::MEDIAN_NS).ln() / m.sigma)
            .collect();
        let mean = x.iter().sum::<f64>() / x.len() as f64;
        let var = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>();
        let cov = x.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>();
        let rho = cov / var;
        println!("lag-1 correlation {rho:.3} (phi 0.7), median {:.3} ms", median / 1e6);
        assert!((rho - 0.7).abs() < 0.02, "lag-1 correlation {rho}");
    }

    /// With sigma 0 the body is exactly the median, so every sample of a run
    /// is median + that run's offset; across runs the offsets have sd
    /// run_sd_ns, and scale multiplies the whole.
    #[test]
    fn run_offset_shifts_whole_run() {
        let m = Synth::new(0.0, 0.0, 3e5);
        let offsets: Vec<f64> = (0..400)
            .map(|seed| {
                let s = latencies(&m.samples(seed, 1.0));
                assert!(s.iter().all(|&v| v == s[0]), "offset varies within run {seed}");
                s[0] as f64 - Synth::MEDIAN_NS
            })
            .collect();
        let mean = offsets.iter().sum::<f64>() / offsets.len() as f64;
        let sd = (offsets.iter().map(|o| (o - mean).powi(2)).sum::<f64>() / (offsets.len() - 1) as f64).sqrt();
        println!("offset sd over 400 runs: {sd:.0} ns (want 300000)");
        assert!((sd / 3e5 - 1.0).abs() < 0.1, "offset sd {sd}");
        let (one, scaled) = (m.samples(9, 1.0)[0].latency_ns, m.samples(9, 1.1)[0].latency_ns);
        assert!(scaled.abs_diff(one * 11 / 10) <= 1, "{one} {scaled}");
    }

    /// A stall adds `extra_ns` to a contiguous `fraction` of a stalled
    /// run, unscaled, in about `prob` of runs, and changes nothing else.
    #[test]
    fn stall_hits_contiguous_fraction_of_some_runs() {
        let plain = Synth::new(0.5, 0.5, 3e5);
        let mut m = plain;
        m.stall = Some(Stall {
            prob: 0.2,
            extra_ns: 300e6,
            fraction: 0.02,
        });
        let mut stalled_runs = 0;
        for seed in 0..1000 {
            let (base, with) = (latencies(&plain.samples(seed, 1.1)), latencies(&m.samples(seed, 1.1)));
            let hit: Vec<usize> = (0..base.len()).filter(|&i| with[i] != base[i]).collect();
            if hit.is_empty() {
                continue;
            }
            stalled_runs += 1;
            assert_eq!(hit.len(), 20, "seed {seed}");
            assert_eq!(hit[19] - hit[0], 19, "seed {seed}: not contiguous");
            assert!(hit.iter().all(|&i| with[i] - base[i] == 300_000_000), "seed {seed}");
        }
        println!("{stalled_runs} of 1000 runs stalled (want about 200)");
        assert!((150..=250).contains(&stalled_runs), "{stalled_runs}");
    }
}
