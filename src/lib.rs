//! tailbench: load-generation runners that record latency into HdrHistogram.
//!
//! This file exists to hold the two runners side by side so the difference
//! between them (which timestamp latency is measured from) stays obvious.
//!
//! Timestamps use `tokio::time::Instant` so the runners follow tokio's paused
//! clock in tests; outside of tests it behaves like `std::time::Instant`.

use std::future::Future;
use std::time::Duration;

use hdrhistogram::Histogram;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};

/// Latency histogram in microseconds.
pub type LatencyHistogram = Histogram<u64>;

/// Highest trackable latency: 1 hour, in microseconds.
const MAX_LATENCY_US: u64 = 3_600_000_000;

fn new_histogram() -> LatencyHistogram {
    Histogram::new_with_bounds(1, MAX_LATENCY_US, 3).expect("valid histogram bounds")
}

fn record(h: &mut LatencyHistogram, latency: Duration) {
    // Saturate instead of failing on out-of-range values; clamp 0 up to 1 us.
    let us = (latency.as_micros() as u64).clamp(1, MAX_LATENCY_US);
    h.record(us).expect("value within histogram bounds");
}

/// One client, one request in flight. Targets one send every `interval`, but
/// never sends before the previous response arrives; if it falls behind it
/// sends immediately. Latency = response time - ACTUAL send time, so time
/// spent waiting to send is invisible (coordinated omission).
pub async fn closed_loop<F, Fut>(interval: Duration, count: u64, mut call: F) -> LatencyHistogram
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut hist = new_histogram();
    let start = Instant::now();
    for i in 0..count {
        sleep_until(start + interval * i as u32).await;
        let sent = Instant::now();
        call().await;
        record(&mut hist, sent.elapsed());
    }
    hist
}

/// Sends one request every `interval` regardless of responses, each in its own
/// task. Latency = response time - INTENDED send time, so a stalled server
/// (or a late sender) shows up in every request it delays.
pub async fn open_loop<F, Fut>(interval: Duration, count: u64, mut call: F) -> LatencyHistogram
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut hist = new_histogram();
    let mut in_flight = JoinSet::new();
    let start = Instant::now();
    for i in 0..count {
        let intended = start + interval * i as u32;
        sleep_until(intended).await;
        let request = call();
        in_flight.spawn(async move {
            request.await;
            intended.elapsed()
        });
    }
    while let Some(latency) = in_flight.join_next().await {
        record(&mut hist, latency.expect("request task panicked"));
    }
    hist
}
