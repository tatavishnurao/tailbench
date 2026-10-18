//! Real-time smoke test: runs open_loop on tokio's REAL clock to see how far
//! actual sends drift from the schedule (scheduler lag) on this machine.
//!
//! Only checks that every request and every lag sample was recorded; it sets
//! no bound on lag yet, because lag depends on the machine and tokio's timer.

use std::time::Duration;

use tailbench::{CountedHistogram, RunConfig, RunnerKind, run};

// 200 requests/s for 5 s; fake server with no stall, 1 ms per request.
const INTERVAL: Duration = Duration::from_millis(5);
const COUNT: u64 = 1_000;
const SERVICE_TIME: Duration = Duration::from_millis(1);

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn row(name: &str, h: &CountedHistogram) {
    println!(
        "{name:<14} {:>5} {:>9.3} {:>9.3} {:>9.3} {:>9} {:>9}",
        h.hist.len(),
        ms(h.hist.value_at_quantile(0.50)),
        ms(h.hist.value_at_quantile(0.99)),
        ms(h.hist.max()),
        h.below_min,
        h.above_max,
    );
}

#[tokio::test]
async fn scheduler_lag_is_recorded_on_the_real_clock() {
    let config = RunConfig {
        kind: RunnerKind::OpenLoop,
        interval: INTERVAL,
        count: COUNT,
        seed: None,
    };
    let mut manifest = Vec::new();
    let wall = std::time::Instant::now();
    let result = run(&config, &mut manifest, || tokio::time::sleep(SERVICE_TIME))
        .await
        .expect("run succeeds");
    let wall = wall.elapsed();

    let service = result.service.as_ref().expect("open loop records service time");
    let lag = result.scheduler_lag.as_ref().expect("open loop records scheduler lag");

    println!("manifest: {}", String::from_utf8_lossy(&manifest).trim());
    println!("wall time: {wall:?}");
    println!(
        "{:<14} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "histogram", "n", "p50 ms", "p99 ms", "max ms", "below_min", "above_max"
    );
    row("scheduler_lag", lag);
    row("service", service);
    row("latency", &result.latency);

    assert_eq!(result.latency.hist.len(), COUNT);
    assert_eq!(service.hist.len(), COUNT);
    assert_eq!(lag.hist.len(), COUNT, "one lag sample per request");
}
