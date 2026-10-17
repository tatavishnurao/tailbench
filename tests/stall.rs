//! Stall test: proves the closed-loop runner hides a 500 ms server freeze
//! (coordinated omission) and the open-loop runner exposes it.
//!
//! Uses an in-process fake server and tokio's paused clock, so it needs no
//! network and runs in milliseconds of wall time with exact timings.

use std::time::Duration;

use tailbench::{CountedHistogram, closed_loop, open_loop};
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
        Self { freeze_start, freeze_end: freeze_start + FREEZE_FOR }
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
    println!(" {:>9.3} {:>9} {:>9}", ms(h.max()), samples.below_min, samples.above_max);
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
    assert!(closed.scheduler_lag.is_none(), "closed loop does not split scheduler lag");

    assert_eq!(open_lag.hist.len(), COUNT, "one lag sample per request");
    assert_eq!(open_lag.below_min + open_lag.above_max, 0, "lag never altered");
    assert_eq!(open_lag.hist.max(), 0, "paused clock: every send exactly on schedule");
}
