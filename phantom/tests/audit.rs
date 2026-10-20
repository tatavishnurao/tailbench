//! Audit: tailbench measures an in-process phantom over real HTTP on
//! 127.0.0.1, and its percentiles are compared with the truth computed from
//! phantom's ground-truth log.
//!
//! Every test is #[ignore]: they run 10 s each on the real clock and their
//! results depend on the machine. Run them with
//!
//! ```sh
//! cargo test -p phantom --test audit -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Truth is the intended-schedule truth defined in `common` (origin at the
//! least-lagged request).
//!
//! Scenario C's reference is scenario B's truth: same stall script and rate,
//! measured by an open-loop (intended-time) observer. Its own log is printed
//! too, labelled, for information only.

mod common;

use common::{
    Audit, COUNT, MAX, NS_PER_MS, P50, P99, P999, PERIOD, Pct, SCENARIO_B, STALL, audit, scenario_b, sorted,
    tailbench_at, truth_at,
};
use tailbench::RunnerKind;

/// Open-loop pass criteria: |delta| <= 5 ms at p99, p99.9 and max, and
/// <= 3 ms at p50. Absolute, so a large truth earns no extra slack.
const TAIL_ABS_NS: f64 = 5.0 * NS_PER_MS;
const P50_ABS_NS: f64 = 3.0 * NS_PER_MS;
/// Closed-loop criterion: tailbench's p99 is below 5% of scenario B's truth p99.
const CLOSED_LOOP_MAX_RATIO: f64 = 0.05;

/// A table row: (percentile, tailbench ns, truth ns).
type Row = (Pct, f64, f64);

/// Prints percentile | tailbench | truth | delta | relative, in ms, with
/// `a`'s tailbench numbers against `truth_ns` (normally `a`'s own truth).
fn table(title: &str, a: &Audit, truth_ns: &[i64], truth_label: &str, pcts: &[Pct]) -> Vec<Row> {
    let sorted = sorted(truth_ns);
    let rows: Vec<Row> = pcts
        .iter()
        .map(|&p| (p, tailbench_at(&a.result, p), truth_at(&sorted, p)))
        .collect();
    println!("\n== {title} ==");
    println!(
        "runner {}, {} requests every {:?}, phantom fixed 1 ms service, {} requests stalled by phantom",
        a.manifest["runner"], COUNT, PERIOD, a.stalled
    );
    println!(
        "{:<10} | {:>12} | {:>12} | {:>12} | {:>9}",
        "percentile", "tailbench ms", truth_label, "delta ms", "relative"
    );
    for &(p, tb, truth) in &rows {
        println!(
            "{:<10} | {:>12.3} | {:>12.3} | {:>+12.3} | {:>+8.2}%",
            p.name,
            tb / NS_PER_MS,
            truth / NS_PER_MS,
            (tb - truth) / NS_PER_MS,
            (tb - truth) / truth * 100.0
        );
    }
    match &a.result.scheduler_lag {
        Some(lag) => println!(
            "tailbench scheduler lag: p99 {:.3} ms, max {:.3} ms ({} samples)",
            lag.hist.value_at_quantile(0.99) as f64 / 1e3,
            lag.hist.max() as f64 / 1e3,
            lag.hist.len()
        ),
        None => println!("tailbench scheduler lag: not recorded (closed_loop records none)"),
    }
    println!(
        "truth origin: least-lagged request, {:.3} ms before client_id 0's arrival",
        a.origin_shift_ns as f64 / NS_PER_MS
    );
    rows
}

/// The manifest describes this run.
fn check_manifest(a: &Audit, runner: &str) {
    let m = &a.manifest;
    assert_eq!(m["runner"], runner, "{m}");
    assert_eq!(m["count"], COUNT, "{m}");
    assert_eq!(m["interval_ns"], PERIOD.as_nanos() as u64, "{m}");
    assert_eq!(m["paused_clock"], false, "{m}");
    assert_eq!(a.result.latency.hist.len(), COUNT);
}

fn check_open_loop(a: &Audit, rows: &[Row]) {
    check_manifest(a, "open_loop");
    let lag = a
        .result
        .scheduler_lag
        .as_ref()
        .expect("open loop records scheduler lag");
    assert_eq!(lag.hist.len(), COUNT, "a scheduler-lag sample per request");
    let failures: Vec<String> = rows
        .iter()
        .filter_map(|&(p, tb, truth)| {
            let delta = (tb - truth).abs();
            let bound = match p.name {
                "p50" => P50_ABS_NS,
                _ => TAIL_ABS_NS,
            };
            (delta > bound).then(|| {
                format!(
                    "{}: |delta| {:.3} ms > bound {:.3} ms",
                    p.name,
                    delta / NS_PER_MS,
                    bound / NS_PER_MS
                )
            })
        })
        .collect();
    assert!(failures.is_empty(), "audit failed: {}", failures.join("; "));
}

#[test]
#[ignore = "timing-sensitive, 10 s on the real clock"]
fn a_control_open_loop() {
    let a = audit(RunnerKind::OpenLoop, vec![]);
    let rows = table(
        "A. control, no stall, open loop",
        &a,
        &a.truth_ns,
        "truth ms",
        &[P50, P99, MAX],
    );
    assert_eq!(a.stalled, 0);
    check_open_loop(&a, &rows);
}

#[test]
#[ignore = "timing-sensitive, 10 s on the real clock"]
fn b_stall_open_loop() {
    let a = scenario_b();
    let rows = table(
        "B. stall 2000:500, open loop",
        a,
        &a.truth_ns,
        "truth ms",
        &[P50, P99, P999, MAX],
    );
    assert!(a.stalled > 0, "precondition: phantom stalled no request");
    check_open_loop(a, &rows);
}

#[test]
#[ignore = "timing-sensitive, 10 s on the real clock"]
fn c_stall_closed_loop() {
    let reused = SCENARIO_B.get().is_some();
    let b = scenario_b();
    let a = audit(RunnerKind::ClosedLoop, vec![STALL]);
    let rows = table(
        "C. stall 2000:500, closed loop (reference: scenario B truth)",
        &a,
        &b.truth_ns,
        "B truth ms",
        &[P99, MAX],
    );
    println!(
        "reference: scenario B's truth, from {} in this process (same stall script and rate, open loop)",
        if reused {
            "the run b_stall_open_loop made"
        } else {
            "a run made for this test"
        }
    );
    let own = sorted(&a.truth_ns);
    println!(
        "C's own log, truth on the open-loop schedule (information only, not the reference): p99 {:.3} ms, max {:.3} ms",
        truth_at(&own, P99) / NS_PER_MS,
        truth_at(&own, MAX) / NS_PER_MS
    );
    assert!(a.stalled > 0, "precondition: phantom stalled no request");
    check_manifest(&a, "closed_loop");
    let (_, tb_p99, truth_p99) = rows[0];
    let ratio = tb_p99 / truth_p99;
    println!(
        "closed-loop p99 is {:.2}% of scenario B's truth p99. Cause: tailbench runner {} (from its manifest) \
         measures latency from the ACTUAL send and sends nothing while a response is outstanding, \
         so the time requests waited to be sent during the stall is not measured (coordinated omission).",
        ratio * 100.0,
        a.manifest["runner"]
    );
    assert!(
        ratio < CLOSED_LOOP_MAX_RATIO,
        "closed-loop p99 {:.3} ms is not below {}% of scenario B's truth p99 {:.3} ms",
        tb_p99 / NS_PER_MS,
        CLOSED_LOOP_MAX_RATIO * 100.0,
        truth_p99 / NS_PER_MS
    );
}
