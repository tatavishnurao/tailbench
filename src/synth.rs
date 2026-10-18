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
///
/// Sample i is intended at i * interval; service = latency and scheduler
/// lag = 0, as on a perfect timer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Synth {
    pub median_ns: f64,
    pub sigma: f64,
    pub phi: f64,
    pub run_sd_ns: f64,
    /// Samples per run.
    pub n: usize,
    pub interval: Duration,
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
            n: 1000,
            interval: Duration::from_millis(10),
        }
    }

    pub fn samples(&self, seed: u64, scale: f64) -> Vec<Sample> {
        let mut rng = SplitMix64::new(seed);
        let offset = self.run_sd_ns * rng.normal();
        let innovation = (1.0 - self.phi * self.phi).sqrt();
        let interval_ns = u64::try_from(self.interval.as_nanos()).expect("interval fits in u64 ns");
        let mut x = rng.normal();
        (0u64..)
            .take(self.n)
            .map(|i| {
                let body = self.median_ns * (self.sigma * x).exp();
                x = self.phi * x + innovation * rng.normal();
                // f64 -> u64 `as` saturates: negative values become 0.
                let ns = (scale * (body + offset)).round() as u64;
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
        let config = RunConfig {
            kind: RunnerKind::OpenLoop,
            interval: self.interval,
            count: self.n as u64,
            seed: None,
            extra: BTreeMap::from([("target".to_owned(), "synth".to_owned())]),
        };
        let mut m = Manifest::collect(&config);
        m.build_git_dirty = Probe::Known(false);
        m.runtime_git_dirty = Probe::Known(false);
        m.sha_mismatch = Probe::Known(false);
        m
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
}
