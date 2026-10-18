//! tailbench: load-generation runners that record latency into HdrHistogram.
//!
//! This file exists to hold the two runners side by side so the difference
//! between them (which timestamp latency is measured from) stays obvious, and
//! `run`, which pins down that a manifest is written before any request.
//!
//! Timestamps use `tokio::time::Instant` so the runners follow tokio's paused
//! clock in tests; outside of tests it behaves like `std::time::Instant`.

use std::future::Future;
use std::io::{self, Write};
use std::time::Duration;

use hdrhistogram::Histogram;
use serde::Serialize;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};

mod git;
pub mod manifest;

use manifest::Manifest;

/// Latency histogram in microseconds.
pub type LatencyHistogram = Histogram<u64>;

/// Lowest latency or service time we accept unaltered, in microseconds: a
/// request that took 0 whole microseconds is a measurement artefact.
const MIN_LATENCY_US: u64 = 1;
/// Lowest scheduler lag we accept unaltered: 0 us is the ideal, so it is kept.
const MIN_LAG_US: u64 = 0;
/// Highest trackable value: 1 hour, in microseconds.
const MAX_LATENCY_US: u64 = 3_600_000_000;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// One HdrHistogram plus counts of the samples that had to be moved into range.
///
/// Every sample is recorded in `hist` exactly once, so `hist.len()` is the
/// number of samples. A value outside [min, MAX_LATENCY_US] is recorded at the
/// nearest bound AND counted in `below_min` or `above_max`, so nothing is
/// altered silently.
#[derive(Debug)]
pub struct CountedHistogram {
    pub hist: LatencyHistogram,
    /// Samples under this histogram's minimum (recorded as the minimum).
    pub below_min: u64,
    /// Samples over 1 h (recorded as 1 h).
    pub above_max: u64,
    min_us: u64,
}

impl CountedHistogram {
    fn new(min_us: u64) -> Self {
        // HdrHistogram's lower bound must be >= 1, but it can still store 0
        // exactly; `min_us` decides what we treat as out of range.
        let hist = Histogram::new_with_bounds(1, MAX_LATENCY_US, 3).expect("valid histogram bounds");
        Self {
            hist,
            below_min: 0,
            above_max: 0,
            min_us,
        }
    }

    fn record(&mut self, value: Duration) {
        // Whole microseconds: the sub-microsecond remainder is truncated.
        let us = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        let us = if us < self.min_us {
            self.below_min += 1;
            self.min_us
        } else if us > MAX_LATENCY_US {
            self.above_max += 1;
            MAX_LATENCY_US
        } else {
            us
        };
        self.hist.record(us).expect("value within histogram bounds");
    }
}

/// What a run measured.
#[derive(Debug)]
pub struct RunResult {
    /// open_loop: t_done - t_intended. closed_loop: t_done - t_sent.
    pub latency: CountedHistogram,
    /// open_loop only: t_done - t_sent, the part of latency spent after the send.
    pub service: Option<CountedHistogram>,
    /// open_loop only: t_sent - t_intended, how late the sender woke up.
    pub scheduler_lag: Option<CountedHistogram>,
}

/// Which runner a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    ClosedLoop,
    OpenLoop,
}

/// Parameters of one run; everything here is copied into the manifest.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub kind: RunnerKind,
    pub interval: Duration,
    pub count: u64,
    /// Recorded in the manifest only; no runner uses randomness yet.
    pub seed: Option<u64>,
}

/// Writes the run manifest to `manifest_out` as JSON and flushes it, and only
/// then starts the runner, so the schedule (and the first request) begins
/// after the manifest is complete. If writing the manifest fails, the error is
/// returned and no request is sent.
///
/// Collecting the manifest runs blocking commands (`git`, `uname`) on this
/// task; that happens before the schedule starts, so it adds no lag.
pub async fn run<W, F, Fut>(config: &RunConfig, mut manifest_out: W, call: F) -> io::Result<RunResult>
where
    W: Write,
    F: FnMut() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let manifest = Manifest::collect(config);
    serde_json::to_writer_pretty(&mut manifest_out, &manifest)?;
    manifest_out.write_all(b"\n")?;
    manifest_out.flush()?;

    Ok(match config.kind {
        RunnerKind::ClosedLoop => closed_loop(config.interval, config.count, call).await,
        RunnerKind::OpenLoop => open_loop(config.interval, config.count, call).await,
    })
}

/// `i * interval`, computed in u128 nanoseconds so no index is truncated;
/// `None` if the result does not fit in a `Duration`.
fn send_offset(interval: Duration, i: u64) -> Option<Duration> {
    let nanos = interval.as_nanos().checked_mul(u128::from(i))?;
    let secs = u64::try_from(nanos / NANOS_PER_SEC).ok()?;
    Some(Duration::new(secs, (nanos % NANOS_PER_SEC) as u32))
}

/// Intended send times `start + i * interval` for `i` in `0..count`.
struct Schedule {
    start: Instant,
    interval: Duration,
}

impl Schedule {
    /// Starts the schedule now. Send times only grow with `i`, so checking the
    /// last one up front means `at` cannot fail mid-run.
    ///
    /// Panics, before anything is sent, if the last send time cannot be
    /// represented as an `Instant`.
    fn start_now(interval: Duration, count: u64) -> Self {
        let schedule = Self {
            start: Instant::now(),
            interval,
        };
        if let Some(last) = count.checked_sub(1) {
            assert!(
                schedule.checked_at(last).is_some(),
                "schedule overflows: {count} sends every {interval:?} cannot be represented"
            );
        }
        schedule
    }

    fn checked_at(&self, i: u64) -> Option<Instant> {
        self.start.checked_add(send_offset(self.interval, i)?)
    }

    fn at(&self, i: u64) -> Instant {
        self.checked_at(i).expect("checked in Schedule::start_now")
    }
}

/// PACED closed loop: one client, at most one request in flight.
///
/// Send `i` is due at `start + i * interval`, but is never sent before the
/// previous response arrives. After a stall the client is behind schedule, so
/// it catches up by sending the missed requests back-to-back (no sleep) until
/// it is on schedule again; no send is skipped, so `count` samples are taken.
///
/// Latency = response time - ACTUAL send time, so time a request spent waiting
/// to be sent is invisible (coordinated omission). Only latency is recorded:
/// `service` and `scheduler_lag` are `None`.
///
/// Panics if the schedule overflows (see `Schedule::start_now`).
pub async fn closed_loop<F, Fut>(interval: Duration, count: u64, mut call: F) -> RunResult
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut latency = CountedHistogram::new(MIN_LATENCY_US);
    let schedule = Schedule::start_now(interval, count);
    for i in 0..count {
        sleep_until(schedule.at(i)).await;
        let sent = Instant::now();
        call().await;
        latency.record(sent.elapsed());
    }
    RunResult {
        latency,
        service: None,
        scheduler_lag: None,
    }
}

/// The three instants of one open-loop request.
struct Timestamps {
    /// When the schedule said to send (computed before the send).
    t_intended: Instant,
    /// When the sender actually woke up and sent.
    t_sent: Instant,
    /// When the response future completed.
    t_done: Instant,
}

/// Open loop on a fixed schedule: send `i` happens at `start + i * interval`
/// whether or not earlier responses have arrived.
///
/// The INTENDED send time is computed before the send; latency = response
/// time - intended send time, so a stalled server (or a sender that wakes
/// late) shows up in every request it delays. Each request runs in its own
/// task, and the set of in-flight tasks is UNBOUNDED: a server that never
/// answers makes it grow by one task per interval.
///
/// Also records, per request, service = t_done - t_sent and scheduler_lag =
/// t_sent - t_intended (so latency = scheduler_lag + service).
///
/// Panics if the schedule overflows (see `Schedule::start_now`).
pub async fn open_loop<F, Fut>(interval: Duration, count: u64, mut call: F) -> RunResult
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut latency = CountedHistogram::new(MIN_LATENCY_US);
    let mut service = CountedHistogram::new(MIN_LATENCY_US);
    let mut scheduler_lag = CountedHistogram::new(MIN_LAG_US);
    let mut in_flight = JoinSet::new();
    let schedule = Schedule::start_now(interval, count);
    for i in 0..count {
        let t_intended = schedule.at(i);
        sleep_until(t_intended).await;
        let t_sent = Instant::now();
        let request = call();
        in_flight.spawn(async move {
            request.await;
            Timestamps {
                t_intended,
                t_sent,
                t_done: Instant::now(),
            }
        });
    }
    while let Some(ts) = in_flight.join_next().await {
        let ts = ts.expect("request task panicked");
        // tokio's Instant subtraction saturates at zero, so none can underflow.
        latency.record(ts.t_done - ts.t_intended);
        service.record(ts.t_done - ts.t_sent);
        scheduler_lag.record(ts.t_sent - ts.t_intended);
    }
    RunResult {
        latency,
        service: Some(service),
        scheduler_lag: Some(scheduler_lag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins down the percentile definition `value_at_quantile` uses with our
    /// histogram configuration (1 us .. 1 h, 3 significant digits):
    ///
    /// nearest rank: with n samples, `value_at_quantile(q)` takes the
    /// `max(1, ceil(q * n))`-th smallest sample and returns the HIGHEST value
    /// in that sample's HdrHistogram bucket. It never interpolates, and it
    /// rounds up by less than one bucket (< value / 1024).
    #[test]
    fn value_at_quantile_is_nearest_rank_rounded_up_to_bucket() {
        let mut samples = CountedHistogram::new(MIN_LATENCY_US);
        for ms in 1..=100u64 {
            samples.record(Duration::from_millis(ms));
        }
        let h = &samples.hist;
        assert_eq!(h.len(), 100);
        // rank ceil(0.50 * 100) = 50 -> 50 000 us; bucket width 32 us -> 50 015.
        assert_eq!(h.value_at_quantile(0.50), 50_015);
        // rank ceil(0.99 * 100) = 99 -> 99 000 us; bucket width 64 us -> 99 007.
        assert_eq!(h.value_at_quantile(0.99), 99_007);
        // rank 100 -> 100 000 us; bucket width 64 us -> 100 031 (same as max()).
        assert_eq!(h.value_at_quantile(1.0), 100_031);
        assert_eq!(h.max(), 100_031);
    }

    #[test]
    fn out_of_range_samples_are_counted_not_hidden() {
        let mut samples = CountedHistogram::new(MIN_LATENCY_US);
        samples.record(Duration::ZERO);
        samples.record(Duration::from_nanos(999)); // 0 whole microseconds
        samples.record(Duration::from_micros(1));
        samples.record(Duration::from_micros(MAX_LATENCY_US));
        samples.record(Duration::from_micros(MAX_LATENCY_US + 1));
        samples.record(Duration::MAX);
        assert_eq!(samples.below_min, 2);
        assert_eq!(samples.above_max, 2);
        assert_eq!(samples.hist.len(), 6, "every sample is still recorded once");
    }

    /// Zero scheduler lag is the ideal case, not an out-of-range value: the lag
    /// histogram stores 0 us as 0 and does not count it in `below_min`.
    #[test]
    fn zero_lag_is_recorded_exactly() {
        let mut lag = CountedHistogram::new(MIN_LAG_US);
        lag.record(Duration::ZERO);
        lag.record(Duration::from_nanos(999));
        assert_eq!(lag.below_min, 0);
        assert_eq!(lag.hist.len(), 2);
        assert_eq!(lag.hist.max(), 0, "both samples stored as exactly 0 us");
    }

    #[test]
    fn send_offset_does_not_truncate_large_indices() {
        let one_ms = Duration::from_millis(1);
        // 5e9 > u32::MAX: `i as u32` would wrap this to 705 032 704.
        assert_eq!(send_offset(one_ms, 5_000_000_000), Some(Duration::from_secs(5_000_000)));
        assert_eq!(send_offset(Duration::MAX, 2), None);
        assert_eq!(send_offset(Duration::MAX, 1), Some(Duration::MAX));
        assert_eq!(send_offset(Duration::MAX, 0), Some(Duration::ZERO));
    }
}
