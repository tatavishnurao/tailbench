//! Stall test: proves the closed-loop runner hides a 500 ms server freeze
//! (coordinated omission) and the open-loop runner exposes it.
//!
//! Uses an in-process fake server and tokio's paused clock, so it needs no
//! network and runs in milliseconds of wall time with exact timings.

use std::time::Duration;

use tailbench::{LatencyHistogram, closed_loop, open_loop};
use tokio::time::{Instant, sleep, sleep_until};

const SERVICE_TIME: Duration = Duration::from_millis(1);
const FREEZE_AT: Duration = Duration::from_secs(2);
const FREEZE_FOR: Duration = Duration::from_millis(500);

// 100 requests/s for 10 s.
const INTERVAL: Duration = Duration::from_millis(10);
const COUNT: u64 = 1_000;

/// Fake server: every request takes SERVICE_TIME, except that a request
/// arriving during [FREEZE_AT, FREEZE_AT + FREEZE_FOR) first waits for the
/// freeze to end. Requests are served concurrently (no queueing between them).
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

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn report(name: &str, h: &LatencyHistogram) {
    let n = h.len();
    println!(
        "{name:<12} p50={:>8.3}ms (n={n})  p95={:>8.3}ms (n={n})  p99={:>8.3}ms (n={n})  max={:>8.3}ms (n={n})",
        ms(h.value_at_quantile(0.50)),
        ms(h.value_at_quantile(0.95)),
        ms(h.value_at_quantile(0.99)),
        ms(h.max()),
    );
}

#[tokio::test(start_paused = true)]
async fn stall_is_hidden_by_closed_loop_and_visible_in_open_loop() {
    let server = FakeServer::starting_now();
    let closed = closed_loop(INTERVAL, COUNT, || server.call()).await;

    let server = FakeServer::starting_now();
    let open = open_loop(INTERVAL, COUNT, || server.call()).await;

    report("closed_loop", &closed);
    report("open_loop", &open);

    // Guard against an empty histogram passing the "< 10 ms" check vacuously.
    assert_eq!(closed.len(), COUNT, "closed_loop recorded wrong count");
    assert_eq!(open.len(), COUNT, "open_loop recorded wrong count");

    let closed_p99 = ms(closed.value_at_quantile(0.99));
    assert!(closed_p99 < 10.0, "closed_loop p99 = {closed_p99} ms, expected < 10 ms");

    // 50 of 1000 requests land in the freeze, with latencies 501, 491, ..., 11 ms.
    // p99 is the 990th smallest sample, i.e. the 40th of those 50: ~401 ms.
    let open_p99 = ms(open.value_at_quantile(0.99));
    assert!(
        (390.0..=420.0).contains(&open_p99),
        "open_loop p99 = {open_p99} ms, expected 390..=420 ms"
    );
}
