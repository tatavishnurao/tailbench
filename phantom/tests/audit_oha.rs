//! Audit of oha, a third-party HTTP load generator, against phantom's ground
//! truth. This is a report, not a pass/fail gate for oha: the only assertions
//! are that oha ran, its JSON parsed, and phantom logged enough requests.
//!
//! oha is an external binary (`cargo install oha --locked`), not a
//! dependency. Every test is #[ignore]; run them with
//!
//! ```sh
//! cargo test --release -p phantom --test audit_oha -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Reference. oha sends no per-request id, so its requests cannot be matched
//! to log lines. The reference is the intended-schedule truth of tailbench's
//! open-loop run against the same script and rate (scenario B of audit.rs,
//! computed by the same code in `common`). It is not an independent
//! measurement of oha's run.
//!
//! oha's JSON reports latencies in seconds (`latencyPercentiles`, and
//! `summary.slowest` for the maximum); they are converted to ms here.

mod common;

use std::process::Command;
use std::sync::OnceLock;

use common::{MAX, NS_PER_MS, P50, P99, P999, Pct, STALL, period_ns, scenario_b, sorted, start_phantom, truth_at};
use serde_json::Value;

/// The oha command shared by every scenario, before the URL.
const OHA_BASE: [&str; 13] = [
    "-q",
    "100",
    "-z",
    "10s",
    "--no-tui",
    "--output-format",
    "json",
    "-m",
    "POST",
    "-d",
    "{}",
    "-H",
    "content-type: application/json",
];
const PCTS: [Pct; 4] = [P50, P99, P999, MAX];
/// The window after the stall ends in which arrivals are counted as a burst.
const BURST_WINDOW_NS: u64 = 50_000_000;
const MIN_LOGGED: usize = 100;

/// Reference truth percentiles in ns, in PCTS order, printed once.
fn reference() -> &'static [f64; 4] {
    static REFERENCE: OnceLock<[f64; 4]> = OnceLock::new();
    REFERENCE.get_or_init(|| {
        let b = scenario_b();
        let truth = sorted(&b.truth_ns);
        let values = PCTS.map(|p| truth_at(&truth, p));
        println!("\n== reference: tailbench open loop, stall 2000:500, intended-schedule truth from phantom's log ==");
        println!(
            "{} requests every {:.0} ms, {} stalled by phantom",
            b.truth_ns.len(),
            period_ns() as f64 / NS_PER_MS,
            b.stalled
        );
        for (p, v) in PCTS.iter().zip(values) {
            println!("{:<6} {:>10.3} ms", p.name, v / NS_PER_MS);
        }
        values
    })
}

/// What phantom logged for one oha run.
struct LogSummary {
    received: usize,
    largest_gap_ns: u64,
    burst_after_stall: usize,
    stalled: usize,
    max_stall_wait_ns: u64,
}

fn summarize_log(text: &str) -> LogSummary {
    let mut lines = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap());
    assert_eq!(lines.next().unwrap()["type"], "manifest");
    let (mut arrivals, mut waits) = (Vec::new(), Vec::new());
    for r in lines {
        arrivals.push(r["arrival_ns"].as_u64().unwrap());
        waits.push(r["stall_wait_ns"].as_u64().unwrap());
    }
    arrivals.sort_unstable();
    let stall_end = (STALL.start_ms + STALL.len_ms) * 1_000_000;
    LogSummary {
        received: arrivals.len(),
        largest_gap_ns: arrivals.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0),
        burst_after_stall: arrivals
            .iter()
            .filter(|&&a| a >= stall_end && a < stall_end + BURST_WINDOW_NS)
            .count(),
        stalled: waits.iter().filter(|&&w| w > 0).count(),
        max_stall_wait_ns: waits.iter().copied().max().unwrap_or(0),
    }
}

/// oha's value for `p` in ns. The JSON is in seconds.
fn oha_at(json: &Value, p: Pct) -> f64 {
    let secs = match p.q {
        Some(_) => &json["latencyPercentiles"][p.name],
        None => &json["summary"]["slowest"],
    };
    secs.as_f64().unwrap_or_else(|| panic!("oha JSON has no {}", p.name)) * 1e9
}

/// Runs oha with `extra` flags against a fresh phantom (stall 2000:500) and
/// prints oha's numbers against the reference and phantom's view of the run.
fn oha_scenario(title: &str, extra: &[&str]) {
    let reference = reference();
    let server = start_phantom(vec![STALL]);
    let url = format!("http://{}/v1/chat/completions", server.phantom.local_addr());
    let args: Vec<&str> = OHA_BASE.iter().chain(extra).copied().chain([url.as_str()]).collect();
    // Blocks this thread only: phantom serves from its own runtime's workers.
    let out = Command::new("oha").args(&args).output().expect("oha on PATH");
    let log = server.finish();

    assert!(
        out.status.success(),
        "oha exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).expect("oha printed JSON");
    let summary = summarize_log(&log);
    assert!(
        summary.received >= MIN_LOGGED,
        "phantom logged {} requests",
        summary.received
    );

    println!("\n== {title} ==");
    println!("oha {}", args.join(" "));
    println!(
        "oha: status codes {}, errors {}",
        json["statusCodeDistribution"], json["errorDistribution"]
    );
    println!(
        "{:<10} | {:>10} | {:>12} | {:>10} | {:>9}",
        "percentile", "oha ms", "reference ms", "delta ms", "relative"
    );
    for (p, &r) in PCTS.iter().zip(reference) {
        let o = oha_at(&json, *p);
        println!(
            "{:<10} | {:>10.3} | {:>12.3} | {:>+10.3} | {:>+8.2}%",
            p.name,
            o / NS_PER_MS,
            r / NS_PER_MS,
            (o - r) / NS_PER_MS,
            (o - r) / r * 100.0
        );
    }
    println!(
        "phantom: {} requests received, largest arrival gap {:.3} ms, {} arrived in the {} ms after the stall ended, \
         {} stalled (max stall_wait {:.3} ms)",
        summary.received,
        summary.largest_gap_ns as f64 / NS_PER_MS,
        summary.burst_after_stall,
        BURST_WINDOW_NS / 1_000_000,
        summary.stalled,
        summary.max_stall_wait_ns as f64 / NS_PER_MS
    );
}

#[test]
#[ignore = "runs the external oha binary for 10 s"]
fn o1_default_connections_uncorrected() {
    oha_scenario("O1. oha, 50 connections (default), no latency correction", &[]);
}

#[test]
#[ignore = "runs the external oha binary for 10 s"]
fn o2_default_connections_corrected() {
    oha_scenario(
        "O2. oha, 50 connections (default), --latency-correction",
        &["--latency-correction"],
    );
}

#[test]
#[ignore = "runs the external oha binary for 10 s"]
fn o3_ten_connections_corrected() {
    oha_scenario(
        "O3. oha, 10 connections, --latency-correction",
        &["--latency-correction", "-c", "10"],
    );
}

#[test]
#[ignore = "runs the external oha binary for 10 s"]
fn o4_one_connection_uncorrected() {
    oha_scenario("O4. oha, 1 connection, no latency correction", &["-c", "1"]);
}
