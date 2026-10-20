//! Stall test: proves the closed-loop runner hides a 500 ms server freeze
//! (coordinated omission) and the open-loop runner exposes it.
//!
//! Uses an in-process fake server and tokio's paused clock, so it needs no
//! network and runs in milliseconds of wall time with exact timings.

use std::time::Duration;

use tailbench::episodes::{self, Threshold, ThresholdRule};
use tailbench::stats::{self, DEFAULT_LEVEL, DEFAULT_RESAMPLES};
use tailbench::{CountedHistogram, RunResult, closed_loop, open_loop};
use tokio::time::{Instant, sleep, sleep_until};

const SERVICE_TIME: Duration = Duration::from_millis(1);
const FREEZE_AT: Duration = Duration::from_secs(2);
const FREEZE_FOR: Duration = Duration::from_millis(500);

// 100 requests/s for 10 s.
const INTERVAL: Duration = Duration::from_millis(10);
const COUNT: u64 = 1_000;

/// Fake server model:
/// - Every request takes SERVICE_TIME (1 ms).
/// - The freeze window is [FREEZE_AT, FREEZE_AT + FREEZE_FOR) = [2000, 2500) ms
///   after the server is created: start inclusive, end exclusive.
/// - A request ARRIVING inside the window waits until the window ends, then
///   takes SERVICE_TIME. A request arriving at or after 2500 ms is not delayed.
/// - Requests are served concurrently and independently: there is no queue,
///   so frozen requests do not delay each other after the freeze ends.
#[derive(Clone, Copy)]
struct FakeServer {
    freeze_start: Instant,
    freeze_end: Instant,
}

impl FakeServer {
    fn starting_now() -> Self {
        let freeze_start = Instant::now() + FREEZE_AT;
        Self {
            freeze_start,
            freeze_end: freeze_start + FREEZE_FOR,
        }
    }

    /// Arrival is taken when `call` is invoked (i.e. at send time), not when
    /// the returned future is first polled.
    fn call(self) -> impl Future<Output = ()> + Send + 'static {
        let arrival = Instant::now();
        async move {
            if arrival >= self.freeze_start && arrival < self.freeze_end {
                sleep_until(self.freeze_end).await;
            }
            sleep(SERVICE_TIME).await;
        }
    }
}

const QUANTILES: [(&str, f64); 4] = [("p50", 0.50), ("p95", 0.95), ("p99", 0.99), ("p99.9", 0.999)];

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn report(name: &str, samples: &CountedHistogram) {
    let h = &samples.hist;
    print!("{name:<20} {:>5}", h.len());
    for (_, q) in QUANTILES {
        print!(" {:>9.3}", ms(h.value_at_quantile(q)));
    }
    println!(
        " {:>9.3} {:>9} {:>9}",
        ms(h.max()),
        samples.below_min,
        samples.above_max
    );
}

/// HdrHistogram with 3 significant digits reports the HIGHEST value of the
/// bucket a sample falls in. For our 1 us lowest unit, values below 2048 us
/// are exact; above that, a bucket is 2^k us wide where value >= 1024 * 2^k,
/// so the reported value r for a true value v satisfies v <= r < v + v/1024
/// (under 0.1% high, never low). The paused clock makes every true latency an
/// exact whole millisecond, so this window is the only slack we allow.
fn assert_hdr_eq(runner: &str, label: &str, got_us: u64, expected_ms: u64) {
    let v = expected_ms * 1000;
    assert!(
        got_us >= v && got_us <= v + v / 1024,
        "{runner} {label} = {got_us} us, expected {v} us (+ up to {} us HdrHistogram rounding)",
        v / 1024
    );
}

fn assert_samples(runner: &str, samples: &CountedHistogram, expected_ms: [u64; 5]) {
    assert_eq!(samples.hist.len(), COUNT, "{runner} recorded wrong count");
    assert_eq!(samples.below_min, 0, "{runner} had samples below histogram minimum");
    assert_eq!(samples.above_max, 0, "{runner} had samples above histogram maximum");
    for ((label, q), want) in QUANTILES.iter().zip(expected_ms) {
        assert_hdr_eq(runner, label, samples.hist.value_at_quantile(*q), want);
    }
    assert_hdr_eq(runner, "max", samples.hist.max(), expected_ms[4]);
}

/// Expected values (percentile = the ceil(q * 1000)-th smallest sample; see
/// the definition pinned in src/lib.rs):
///
/// Sends are intended at 0, 10, ..., 9990 ms. Those intended at 2000, 2010,
/// ..., 2490 ms (50 requests) fall in the freeze.
///
/// closed_loop: the request sent at 2000 ms finishes at 2501 ms (501 ms). The
/// 49 sends it delayed then go back-to-back from 2501 ms, after the freeze, at
/// 1 ms each. So 999 samples of 1 ms and 1 of 501 ms:
///   p50, p95, p99, p99.9 (ranks 500, 950, 990, 999) = 1 ms; max = 501 ms.
///
/// open_loop: each frozen request intended at t waits until 2500 ms, then
/// takes 1 ms: latency 2501 - t = 501, 491, ..., 11 ms. The other 950 take 1 ms.
/// Sorted: 950 x 1 ms, then 11, 21, ..., 501 ms.
///   p50 (rank 500) = 1 ms
///   p95 (rank 950, the last 1 ms sample) = 1 ms
///   p99 (rank 990 = 40th frozen) = 11 + 39 * 10 = 401 ms
///   p99.9 (rank 999 = 49th frozen) = 11 + 48 * 10 = 491 ms
///   max = 501 ms
///
/// open_loop service and scheduler lag: on the paused clock, `sleep_until`
/// returns exactly at the intended time, so every send has 0 lag. Then
/// service = latency - lag = latency for every request, so service has the
/// same percentiles as latency.
#[tokio::test(start_paused = true)]
async fn stall_is_hidden_by_closed_loop_and_visible_in_open_loop() {
    let server = FakeServer::starting_now();
    let closed = closed_loop(INTERVAL, COUNT, || server.call()).await;

    let server = FakeServer::starting_now();
    let open = open_loop(INTERVAL, COUNT, || server.call()).await;

    let open_service = open.service.as_ref().expect("open loop records service time");
    let open_lag = open.scheduler_lag.as_ref().expect("open loop records scheduler lag");

    println!(
        "{:<20} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "histogram", "n", "p50 ms", "p95 ms", "p99 ms", "p99.9 ms", "max ms", "below_min", "above_max"
    );
    report("closed latency", &closed.latency);
    report("open latency", &open.latency);
    report("open service", open_service);
    report("open scheduler_lag", open_lag);

    assert_samples("closed_loop latency", &closed.latency, [1, 1, 1, 1, 501]);
    assert_samples("open_loop latency", &open.latency, [1, 1, 401, 491, 501]);
    assert_samples("open_loop service", open_service, [1, 1, 401, 491, 501]);

    assert!(closed.service.is_none(), "closed loop does not split service time");
    assert!(
        closed.scheduler_lag.is_none(),
        "closed loop does not split scheduler lag"
    );

    assert_eq!(open_lag.hist.len(), COUNT, "one lag sample per request");
    assert_eq!(open_lag.below_min + open_lag.above_max, 0, "lag never altered");
    assert_eq!(open_lag.hist.max(), 0, "paused clock: every send exactly on schedule");
}

const MS: u64 = 1_000_000;

/// Per-request samples on the same stall run, in intended-send order.
///
/// open_loop: send i is intended at i * 10 ms. Sends 200..250 (intended
/// 2000..=2490 ms) are frozen: latency = 2501 ms - i * 10 ms. Everything else
/// takes 1 ms. Lag is 0 on the paused clock, so service = latency.
///
/// closed_loop: intended offsets are the same schedule; latency is measured
/// from the actual send, so only send 200 (sent at 2000 ms) shows the freeze
/// (501 ms). It has no service or lag split.
#[tokio::test(start_paused = true)]
async fn samples_are_in_intended_order_with_exact_values() {
    let server = FakeServer::starting_now();
    let closed = closed_loop(INTERVAL, COUNT, || server.call()).await;
    let server = FakeServer::starting_now();
    let open = open_loop(INTERVAL, COUNT, || server.call()).await;

    assert_eq!(open.samples.len() as u64, COUNT);
    for (i, s) in (0u64..).zip(&open.samples) {
        let want = if (200..250).contains(&i) {
            2501 * MS - i * 10 * MS
        } else {
            MS
        };
        assert_eq!(s.intended_offset_ns, i * 10 * MS, "open send {i} out of order");
        assert_eq!(s.latency_ns, want, "open send {i} latency");
        assert_eq!(s.service_ns, Some(want), "open send {i} service");
        assert_eq!(s.scheduler_lag_ns, Some(0), "open send {i} lag");
    }

    assert_eq!(closed.samples.len() as u64, COUNT);
    for (i, s) in (0u64..).zip(&closed.samples) {
        let want = if i == 200 { 501 * MS } else { MS };
        assert_eq!(s.intended_offset_ns, i * 10 * MS, "closed send {i} out of order");
        assert_eq!(s.latency_ns, want, "closed send {i} latency");
        assert_eq!(s.service_ns, None);
        assert_eq!(s.scheduler_lag_ns, None);
    }
}

/// `percentile` on raw samples picks the same sample as `value_at_quantile`
/// on the histogram, so after truncating ns to whole us (as the histogram
/// does), the histogram value is that sample rounded up to its bucket.
#[tokio::test(start_paused = true)]
async fn percentile_agrees_with_histograms_on_stall_data() {
    let server = FakeServer::starting_now();
    let closed = closed_loop(INTERVAL, COUNT, || server.call()).await;
    let server = FakeServer::starting_now();
    let open = open_loop(INTERVAL, COUNT, || server.call()).await;

    let series: [(&str, &CountedHistogram, Vec<u64>); 4] = [
        (
            "closed latency",
            &closed.latency,
            column(&closed, |s| Some(s.latency_ns)),
        ),
        ("open latency", &open.latency, column(&open, |s| Some(s.latency_ns))),
        (
            "open service",
            open.service.as_ref().unwrap(),
            column(&open, |s| s.service_ns),
        ),
        (
            "open lag",
            open.scheduler_lag.as_ref().unwrap(),
            column(&open, |s| s.scheduler_lag_ns),
        ),
    ];
    for (name, h, ns) in &series {
        for (label, q) in QUANTILES.into_iter().chain([("max", 1.0)]) {
            let v = stats::percentile(ns, q).unwrap() / 1000;
            let got = h.hist.value_at_quantile(q);
            assert!(
                got >= v && got <= v + v / 1024,
                "{name} {label}: histogram {got} us, raw {v} us"
            );
        }
    }
}

fn column(result: &RunResult, f: impl Fn(&tailbench::Sample) -> Option<u64>) -> Vec<u64> {
    result.samples.iter().map(|s| f(s).expect("field recorded")).collect()
}

const CI_SEED: u64 = 0x5EED;

fn ms_f(ns: u64) -> f64 {
    ns as f64 / 1e6
}

/// The 50 frozen requests are one contiguous run in intended-send order.
/// Block resampling keeps that run in clumps, so how many slow samples a
/// resample gets varies more than under iid resampling, and the p99 CI must
/// be at least as wide.
#[tokio::test(start_paused = true)]
async fn p99_ci_on_stall_run_mbb_is_at_least_as_wide_as_iid() {
    let server = FakeServer::starting_now();
    let open = open_loop(INTERVAL, COUNT, || server.call()).await;
    let latency = column(&open, |s| Some(s.latency_ns));
    let block_len = stats::default_block_len(latency.len());

    let mbb = stats::mbb_ci(&latency, 0.99, block_len, DEFAULT_RESAMPLES, CI_SEED, DEFAULT_LEVEL).unwrap();
    let iid = stats::iid_ci(&latency, 0.99, DEFAULT_RESAMPLES, CI_SEED, DEFAULT_LEVEL).unwrap();

    println!(
        "open latency p99, n = {}, block_len = {block_len}, resamples = {DEFAULT_RESAMPLES}, level = {DEFAULT_LEVEL}",
        latency.len()
    );
    for (name, ci) in [("mbb", &mbb), ("iid", &iid)] {
        println!(
            "{name}: estimate {:.3} ms, CI [{:.3}, {:.3}] ms, width {:.3} ms, warnings {:?}",
            ms_f(ci.estimate),
            ms_f(ci.lo),
            ms_f(ci.hi),
            ms_f(ci.hi - ci.lo),
            ci.warnings
        );
    }

    assert_eq!(
        mbb.estimate,
        401 * MS,
        "p99 = 40th frozen request, as in the histogram test"
    );
    assert_eq!(iid.estimate, mbb.estimate);
    assert!(
        mbb.warnings.is_empty() && iid.warnings.is_empty(),
        "support is exactly 10"
    );
    assert!(
        mbb.hi - mbb.lo >= iid.hi - iid.lo,
        "mbb CI {:?} narrower than iid CI {:?}",
        (mbb.lo, mbb.hi),
        (iid.lo, iid.hi)
    );
}

/// Split at the default threshold, the freeze is one episode and the body is
/// the run the freeze interrupted.
///
/// Threshold: 950 of the 1000 samples are 1 ms, so the pooled median is
/// 1 ms and T = 10 x 1 ms = 10 ms. The 50 frozen samples (sends 200..250,
/// 11 to 501 ms) are all >= 10 ms and consecutive, so they are one episode.
/// It is open from send 200's intended time, 2000 ms, to the frozen
/// requests' completion at 2501 ms: 501 ms of the run's 9991 ms (the last
/// send, at 9990 ms, completes at 9991 ms).
///
/// Body: the samples sent outside that window [2000, 2501) ms, so the 50
/// frozen ones and send 250 (intended at 2500 ms, 1 ms) are left out: 949
/// samples, all 1 ms, so the body p99 is 1 ms. Without the freeze every
/// request would take 1 ms, so here taking the episode out biases the body
/// p99 by 0.
#[tokio::test(start_paused = true)]
async fn freeze_is_one_episode_and_body_p99_is_1ms() {
    let server = FakeServer::starting_now();
    let open = open_loop(INTERVAL, COUNT, || server.call()).await;
    let t = Threshold::derive(&[&open.samples], ThresholdRule::default()).unwrap();
    let e = episodes::run_episodes(&open.samples, t.ns);
    let body = column_of(&episodes::body(&open.samples, t.ns));
    let p99 = stats::percentile(&body, 0.99).unwrap();
    println!(
        "threshold {t}; {} episode(s), {} samples, {:.3} ms of {:.3} ms; body n = {}, body p99 {:.3} ms (all samples: {:.3} ms)",
        e.episodes,
        e.samples,
        ms_f(e.time_ns),
        ms_f(e.span_ns),
        body.len(),
        ms_f(p99),
        ms_f(stats::percentile(&column(&open, |s| Some(s.latency_ns)), 0.99).unwrap())
    );
    assert_eq!((t.pooled_median_ns, t.ns), (MS, 10 * MS));
    assert_eq!(
        e,
        episodes::RunEpisodes {
            episodes: 1,
            samples: 50,
            excluded: 51,
            time_ns: 501 * MS,
            span_ns: 9991 * MS,
        }
    );
    assert_eq!(body.len(), 949);
    assert_eq!(p99, MS);
}

fn column_of(samples: &[tailbench::Sample]) -> Vec<u64> {
    samples.iter().map(|s| s.latency_ns).collect()
}
