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
//! Truth. Request `i` carries `x-request-id: i`, which phantom logs as
//! `client_id`. Its intended send time is `i * PERIOD` after tailbench's
//! schedule start. tailbench's clock and phantom's are aligned at the arrival
//! of client_id 0, so
//!
//! `truth_i = sent_ns_i - (arrival_ns(client_id 0) + i * PERIOD)`.
//!
//! The alignment has about 1 ms of error: client_id 0 arrives after
//! tailbench's schedule start by its scheduler lag (tokio's timer has
//! millisecond granularity) plus the loopback trip to the handler, so truth
//! reads low by that much. tailbench's latency also includes writing and
//! reading the response, which phantom's sent_ns does not.
//!
//! phantom and tailbench run on separate tokio runtimes, so the server's
//! timers and the client's schedule do not share worker threads.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use phantom::{Script, ServiceSpec, Stall};
use serde_json::Value;
use tailbench::{RunConfig, RunResult, RunnerKind, run};
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};

/// 100 requests/s for 10 s.
const PERIOD: Duration = Duration::from_millis(10);
const COUNT: u64 = 1_000;
const SERVICE: ServiceSpec = ServiceSpec::Fixed { ms: 1 };
const STALL: Stall = Stall {
    start_ms: 2_000,
    len_ms: 500,
};
const BODY: &str = r#"{"model":"any","messages":[{"role":"user","content":"hi"}]}"#;

const NS_PER_MS: f64 = 1e6;
/// Open-loop pass criteria: |delta| <= max(5 ms, 5% of truth) at p99, p99.9
/// and max, and <= 3 ms at p50.
const TAIL_ABS_NS: f64 = 5.0 * NS_PER_MS;
const TAIL_REL: f64 = 0.05;
const P50_ABS_NS: f64 = 3.0 * NS_PER_MS;
/// Closed-loop criterion: tailbench's p99 is below 5% of the truth p99.
const CLOSED_LOOP_MAX_RATIO: f64 = 0.05;

/// A percentile to compare; `None` is the maximum.
#[derive(Clone, Copy)]
struct Pct {
    name: &'static str,
    q: Option<f64>,
}

const P50: Pct = Pct {
    name: "p50",
    q: Some(0.50),
};
const P99: Pct = Pct {
    name: "p99",
    q: Some(0.99),
};
const P999: Pct = Pct {
    name: "p99.9",
    q: Some(0.999),
};
const MAX: Pct = Pct { name: "max", q: None };

/// One scenario's results: what tailbench measured, the truth per request
/// (indexed by client_id), and the manifest tailbench wrote.
struct Audit {
    result: RunResult,
    truth_ns: Vec<i64>,
    manifest: Value,
    stalled: usize,
}

fn runtime(name: &str) -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name(name)
        .enable_all()
        .build()
        .unwrap()
}

/// Starts phantom with `stalls`, runs tailbench's `kind` runner against it
/// for COUNT requests every PERIOD, shuts phantom down and computes truth from
/// its log.
fn audit(kind: RunnerKind, stalls: Vec<Stall>) -> Audit {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("gt.jsonl");
    let manifest_path = dir.path().join("tailbench-manifest.json");

    let server_rt = runtime("phantom");
    let script = Script::new(1, SERVICE, stalls).unwrap();
    let phantom = server_rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        phantom::start(listener, script, &log_path).await.unwrap()
    });
    let base = format!("http://{}", phantom.local_addr());

    let client_rt = runtime("tailbench");
    let result = client_rt.block_on(async {
        // One client for every request; reqwest keeps connections alive.
        let client = reqwest::Client::new();
        // Opens the first connection before the schedule starts, so client_id
        // 0 does not pay for a TCP connect. Health checks are not logged and
        // do not set phantom's t0.
        let resp = client.get(format!("{base}/healthz")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        resp.bytes().await.unwrap();

        let config = RunConfig {
            kind,
            interval: PERIOD,
            count: COUNT,
            seed: None,
            extra: BTreeMap::from([("target".to_owned(), "phantom".to_owned())]),
        };
        let url = format!("{base}/v1/chat/completions");
        let mut next_id = 0u64;
        let call = move || {
            let id = next_id;
            next_id += 1;
            let (client, url) = (client.clone(), url.clone());
            async move {
                let resp = client
                    .post(url)
                    .header("content-type", "application/json")
                    .header("x-request-id", id)
                    .body(BODY)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 200, "client_id {id}");
                resp.bytes().await.unwrap();
            }
        };
        run(&config, File::create(&manifest_path).unwrap(), call).await.unwrap()
    });
    server_rt.block_on(phantom.shutdown()).unwrap();

    let manifest = read_manifest(&manifest_path);
    let (truth_ns, stalled) = truth_from_log(&log_path);
    Audit {
        result,
        truth_ns,
        manifest,
        stalled,
    }
}

fn read_manifest(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("tailbench manifest was written");
    serde_json::from_str(&text).expect("tailbench manifest is JSON")
}

/// Truth latency per client_id, and how many requests phantom stalled.
fn truth_from_log(path: &Path) -> (Vec<i64>, usize) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap());
    assert_eq!(lines.next().unwrap()["type"], "manifest");
    // (arrival_ns, sent_ns) by client_id.
    let mut by_id: Vec<Option<(u64, u64)>> = vec![None; COUNT as usize];
    let mut stalled = 0;
    for r in lines {
        let id = r["client_id"].as_u64().unwrap_or_else(|| panic!("no client_id: {r}")) as usize;
        let times = (r["arrival_ns"].as_u64().unwrap(), r["sent_ns"].as_u64().unwrap());
        assert!(by_id[id].replace(times).is_none(), "client_id {id} logged twice");
        if r["stall_wait_ns"].as_u64().unwrap() > 0 {
            stalled += 1;
        }
    }
    let by_id: Vec<(u64, u64)> = by_id
        .into_iter()
        .enumerate()
        .map(|(i, t)| t.unwrap_or_else(|| panic!("client_id {i} not in the log")))
        .collect();
    let aligned_start = by_id[0].0 as i64;
    let period = PERIOD.as_nanos() as i64;
    let truth = by_id
        .iter()
        .zip(0i64..)
        .map(|(&(_, sent), i)| sent as i64 - (aligned_start + i * period))
        .collect();
    (truth, stalled)
}

/// Nearest rank, the definition tailbench's histogram uses (without its
/// rounding up to the bucket, which is under 0.1%).
fn truth_at(sorted: &[i64], p: Pct) -> f64 {
    let n = sorted.len();
    let rank = match p.q {
        Some(q) => ((q * n as f64).ceil() as usize).max(1),
        None => n,
    };
    sorted[rank - 1] as f64
}

fn tailbench_at(result: &RunResult, p: Pct) -> f64 {
    let h = &result.latency.hist;
    let us = match p.q {
        Some(q) => h.value_at_quantile(q),
        None => h.max(),
    };
    us as f64 * 1e3
}

/// A table row: (percentile, tailbench ns, truth ns).
type Row = (Pct, f64, f64);

/// Prints percentile | tailbench | truth | delta | relative, in ms.
fn table(title: &str, a: &Audit, pcts: &[Pct]) -> Vec<Row> {
    let mut sorted = a.truth_ns.clone();
    sorted.sort_unstable();
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
        "percentile", "tailbench ms", "truth ms", "delta ms", "relative"
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
    println!("truth aligned at client_id 0's arrival: about 1 ms of alignment error");
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
                _ => TAIL_ABS_NS.max(TAIL_REL * truth),
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
    let rows = table("A. control, no stall, open loop", &a, &[P50, P99, MAX]);
    assert_eq!(a.stalled, 0);
    check_open_loop(&a, &rows);
}

#[test]
#[ignore = "timing-sensitive, 10 s on the real clock"]
fn b_stall_open_loop() {
    let a = audit(RunnerKind::OpenLoop, vec![STALL]);
    let rows = table("B. stall 2000:500, open loop", &a, &[P50, P99, P999, MAX]);
    assert!(a.stalled > 0, "precondition: phantom stalled no request");
    check_open_loop(&a, &rows);
}

#[test]
#[ignore = "timing-sensitive, 10 s on the real clock"]
fn c_stall_closed_loop() {
    let a = audit(RunnerKind::ClosedLoop, vec![STALL]);
    let rows = table(
        "C. stall 2000:500, closed loop (truth on the open-loop schedule)",
        &a,
        &[P99, MAX],
    );
    assert!(a.stalled > 0, "precondition: phantom stalled no request");
    check_manifest(&a, "closed_loop");
    let (_, tb_p99, truth_p99) = rows[0];
    let ratio = tb_p99 / truth_p99;
    println!(
        "closed-loop p99 is {:.2}% of the truth p99. Cause: tailbench runner {} (from its manifest) \
         measures latency from the ACTUAL send and sends nothing while a response is outstanding, \
         so the time requests waited to be sent during the stall is not measured (coordinated omission).",
        ratio * 100.0,
        a.manifest["runner"]
    );
    assert!(
        ratio < CLOSED_LOOP_MAX_RATIO,
        "closed-loop p99 {:.3} ms is not below {}% of truth p99 {:.3} ms",
        tb_p99 / NS_PER_MS,
        CLOSED_LOOP_MAX_RATIO * 100.0,
        truth_p99 / NS_PER_MS
    );
}
