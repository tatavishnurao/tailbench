//! Audit of oha, a third-party HTTP load generator, against phantom's ground
//! truth. This is a report, not a pass/fail gate for oha: the only assertions
//! are that oha ran, its JSON parsed, and phantom logged enough requests.
//!
//! oha is an external binary (`cargo install oha --locked`), not a
//! dependency. Every test is #[ignore]; run them with
//!
//! ```sh
//! cargo test --release -p tailbench-phantom --test audit_oha -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Primary comparison: per-run truth. oha sends no per-request id, so its
//! requests are keyed by phantom's seq (arrival order) and run through the
//! same truth formula as tailbench's (`common::truth_from_log`, `Key::Seq`;
//! the ordering assumption is stated there).
//!
//! Secondary: the cross-run reference, the truth of tailbench's own open-loop
//! run against the same script and rate (scenario B of audit.rs). It is a
//! different run, so it is not an independent measurement of oha's run.
//!
//! Percentile conventions differ. This audit uses nearest rank (index
//! `ceil(q * n) - 1`). oha 1.16.0 takes index `floor(p / 100 * n)`
//! (src/result_data.rs, `percentile_iter`), one rank higher at p50, p99 and
//! p99.9 when n = 1000, so per-run truth is also shown at oha's index.
//!
//! oha's JSON reports latencies in seconds (`latencyPercentiles`, and
//! `summary.slowest` for the maximum); they are converted to ns here.

mod common;

use std::process::Command;
use std::sync::OnceLock;

use common::{
    Key, MAX, NS_PER_MS, P50, P99, P999, Pct, STALL, period_ns, scenario_b, sorted, start_phantom, truth_at,
    truth_from_log,
};
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

/// Cross-run reference percentiles in ns, in PCTS order. Prints the machine
/// line and the reference once.
fn reference() -> &'static [f64; 4] {
    static REFERENCE: OnceLock<[f64; 4]> = OnceLock::new();
    REFERENCE.get_or_init(|| {
        let b = scenario_b();
        let m = &b.manifest;
        println!(
            "\nmachine: {}, kernel {}, {}, build {} (opt-level {}), tokio {} (from Cargo.lock: tailbench's manifest \
             records no tokio version)",
            m["cpu_model"],
            m["kernel"],
            m["rust_version"],
            m["build_profile"],
            m["opt_level"],
            locked_tokio_version()
        );
        let truth = sorted(&b.truth_ns);
        let values = PCTS.map(|p| truth_at(&truth, p));
        println!("== cross-run reference: tailbench open loop, stall 2000:500, truth from phantom's log ==");
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

/// tokio's version in the workspace lockfile.
fn locked_tokio_version() -> String {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.lock")).unwrap();
    lock.split_once("name = \"tokio\"\nversion = \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or_else(|| "UNKNOWN".to_owned(), |(version, _)| version.to_owned())
}

/// Phantom-side counts for one oha run that the truth does not show.
struct LogSummary {
    largest_gap_ns: u64,
    burst_after_stall: usize,
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
        largest_gap_ns: arrivals.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0),
        burst_after_stall: arrivals
            .iter()
            .filter(|&&a| a >= stall_end && a < stall_end + BURST_WINDOW_NS)
            .count(),
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

/// `sorted` at oha 1.16.0's index, `(p / 100 * n) as usize`, with oha's own
/// percent literals so the float arithmetic matches.
fn at_oha_index(sorted: &[i64], p: Pct) -> f64 {
    let percent = match p.name {
        "p50" => 50.0,
        "p99" => 99.0,
        "p99.9" => 99.9,
        _ => return sorted[sorted.len() - 1] as f64,
    };
    let i = (percent / 100.0 * sorted.len() as f64) as usize;
    sorted[i.min(sorted.len() - 1)] as f64
}

fn ms(ns: f64) -> f64 {
    ns / NS_PER_MS
}

/// Runs oha with `extra` flags against a fresh phantom (stall 2000:500) and
/// prints oha's numbers against this run's truth, the cross-run reference,
/// and phantom's view of the run.
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
    let truth = truth_from_log(&log, Key::Seq);
    assert!(
        truth.truth_ns.len() >= MIN_LOGGED,
        "phantom logged {} requests",
        truth.truth_ns.len()
    );
    let summary = summarize_log(&log);
    let run_truth = sorted(&truth.truth_ns);

    println!("\n== {title} ==");
    println!("oha {args:?}");
    println!(
        "oha: status codes {}, errors {}",
        json["statusCodeDistribution"], json["errorDistribution"]
    );
    let deadline_aborts = json["errorDistribution"]["aborted due to deadline"]
        .as_u64()
        .unwrap_or(0);
    println!(
        "phantom: {} \"completed\": false lines; oha \"aborted due to deadline\": {}; equal: {}",
        truth.abandoned,
        deadline_aborts,
        truth.abandoned as u64 == deadline_aborts
    );
    println!(
        "{:<6} | {:>9} | {:>13} | {:>8} | {:>8} | {:>14} | {:>8} | {:>15} | {:>8}",
        "pct", "oha ms", "run truth ms", "delta", "relative", "truth@oha-idx", "delta", "cross-run ref", "delta"
    );
    for (p, &cross) in PCTS.iter().zip(reference) {
        let o = oha_at(&json, *p);
        let t = truth_at(&run_truth, *p);
        let t_oha = at_oha_index(&run_truth, *p);
        println!(
            "{:<6} | {:>9.3} | {:>13.3} | {:>+8.3} | {:>+7.2}% | {:>14.3} | {:>+8.3} | {:>15.3} | {:>+8.3}",
            p.name,
            ms(o),
            ms(t),
            ms(o - t),
            (o - t) / t * 100.0,
            ms(t_oha),
            ms(o - t_oha),
            ms(cross),
            ms(o - cross)
        );
    }
    let lateness = sorted(&truth.lateness_ns);
    let late = lateness.iter().filter(|&&l| l > period_ns()).count();
    println!(
        "send lateness (arrival - intended): p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms; {} of {} later than one period",
        ms(truth_at(&lateness, P50)),
        ms(truth_at(&lateness, P99)),
        ms(truth_at(&lateness, MAX)),
        late,
        lateness.len()
    );
    println!(
        "phantom: {} requests completed, largest arrival gap {:.3} ms, {} arrived in the {} ms after the stall ended, \
         {} stalled (max stall_wait {:.3} ms)",
        truth.truth_ns.len(),
        ms(summary.largest_gap_ns as f64),
        summary.burst_after_stall,
        BURST_WINDOW_NS / 1_000_000,
        truth.stalled,
        ms(summary.max_stall_wait_ns as f64)
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

#[test]
#[ignore = "runs the external oha binary for 10 s"]
fn o5_one_connection_corrected() {
    oha_scenario(
        "O5. oha, 1 connection, --latency-correction",
        &["-c", "1", "--latency-correction"],
    );
}
