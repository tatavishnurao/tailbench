//! Shared by the audits (audit.rs, audit_oha.rs): starting phantom, running
//! tailbench against it, and computing intended-schedule truth from
//! phantom's log. Each test binary uses a different subset.
//!
//! Truth. Request `i` is the `i`-th send of the schedule, keyed in phantom's
//! log either by `client_id` (tailbench sends `x-request-id: i`) or by `seq`
//! (arrival order, for a load generator that sends no id; see `Key`). Its
//! intended send time is `i * PERIOD` after the schedule start, which is
//! placed on phantom's clock at
//!
//! `origin = min over i of (arrival_ns_i - i * PERIOD)`
//!
//! (see `truth_from_log` for why), so
//!
//! `truth_i = sent_ns_i - (origin + i * PERIOD)`.
//!
//! Truth still reads low by the smallest per-request delay between intended
//! send and phantom's handler entry, and tailbench's latency also includes
//! writing and reading the response, which phantom's sent_ns does not.
//!
//! phantom and tailbench run on separate tokio runtimes, so the server's
//! timers and the client's schedule do not share worker threads.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use phantom::{Phantom, Script, ServiceSpec, Stall};
use serde_json::Value;
use tailbench::{RunConfig, RunResult, RunnerKind, run};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};

/// 100 requests/s for 10 s.
pub const PERIOD: Duration = Duration::from_millis(10);
pub const COUNT: u64 = 1_000;
pub const SERVICE: ServiceSpec = ServiceSpec::Fixed { ms: 1 };
pub const SEED: u64 = 1;
pub const STALL: Stall = Stall {
    start_ms: 2_000,
    len_ms: 500,
};
const BODY: &str = r#"{"model":"any","messages":[{"role":"user","content":"hi"}]}"#;

pub const NS_PER_MS: f64 = 1e6;

pub fn period_ns() -> i64 {
    PERIOD.as_nanos() as i64
}

/// A percentile to compare; `None` is the maximum.
#[derive(Clone, Copy)]
pub struct Pct {
    pub name: &'static str,
    pub q: Option<f64>,
}

pub const P50: Pct = Pct {
    name: "p50",
    q: Some(0.50),
};
pub const P99: Pct = Pct {
    name: "p99",
    q: Some(0.99),
};
pub const P999: Pct = Pct {
    name: "p99.9",
    q: Some(0.999),
};
pub const MAX: Pct = Pct { name: "max", q: None };

/// One scenario's results: what tailbench measured, the truth per request
/// (indexed by client_id), and the manifest tailbench wrote.
pub struct Audit {
    pub result: RunResult,
    pub truth_ns: Vec<i64>,
    /// How far the chosen origin is before client_id 0's arrival (>= 0): what
    /// aligning on client_id 0 would have added to every truth value's error.
    pub origin_shift_ns: i64,
    pub manifest: Value,
    pub stalled: usize,
}

/// Scenario B's run (open loop, stall 2000:500), shared by every test in one
/// test binary that needs it: whichever runs first runs it.
pub static SCENARIO_B: OnceLock<Audit> = OnceLock::new();

pub fn scenario_b() -> &'static Audit {
    SCENARIO_B.get_or_init(|| audit(RunnerKind::OpenLoop, vec![STALL]))
}

fn runtime(name: &str) -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name(name)
        .enable_all()
        .build()
        .unwrap()
}

/// An in-process phantom on 127.0.0.1:0, serving from its own runtime.
pub struct Server {
    pub phantom: Phantom,
    rt: Runtime,
    log_path: PathBuf,
    _dir: TempDir,
}

/// Starts phantom (fixed 1 ms service, seed SEED) with `stalls`.
pub fn start_phantom(stalls: Vec<Stall>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("gt.jsonl");
    let rt = runtime("phantom");
    let script = Script::new(SEED, SERVICE, stalls).unwrap();
    let phantom = rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        phantom::start(listener, script, &log_path).await.unwrap()
    });
    Server {
        phantom,
        rt,
        log_path,
        _dir: dir,
    }
}

impl Server {
    /// Shuts phantom down and returns its complete log.
    pub fn finish(self) -> String {
        self.rt.block_on(self.phantom.shutdown()).unwrap();
        std::fs::read_to_string(&self.log_path).unwrap()
    }
}

/// Starts phantom with `stalls`, runs tailbench's `kind` runner against it
/// for COUNT requests every PERIOD, shuts phantom down and computes truth from
/// its log.
pub fn audit(kind: RunnerKind, stalls: Vec<Stall>) -> Audit {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("tailbench-manifest.json");
    let server = start_phantom(stalls);
    let base = format!("http://{}", server.phantom.local_addr());

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
    let log = server.finish();

    let manifest = read_manifest(&manifest_path);
    let truth = truth_from_log(&log, Key::ClientId);
    assert_eq!(truth.abandoned, 0, "tailbench abandoned no request");
    assert_eq!(truth.truth_ns.len(), COUNT as usize, "every client_id is in the log");
    Audit {
        result,
        truth_ns: truth.truth_ns,
        origin_shift_ns: truth.origin_shift_ns,
        manifest,
        stalled: truth.stalled,
    }
}

fn read_manifest(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("tailbench manifest was written");
    serde_json::from_str(&text).expect("tailbench manifest is JSON")
}

/// Which log field gives a request's index `i` in the send schedule.
#[derive(Clone, Copy, Debug)]
pub enum Key {
    /// `client_id`, from the `x-request-id` header tailbench sends.
    ClientId,
    /// phantom's `seq`, its arrival order. Assumes the load generator sends in
    /// schedule order and requests arrive in the order they were sent. Inside
    /// a burst (after a stall, say) requests that arrive together may be
    /// permuted: that changes individual truth values, but by no more than
    /// the burst's spread, and leaves the set of intended times unchanged.
    Seq,
}

impl Key {
    fn field(self) -> &'static str {
        match self {
            Key::ClientId => "client_id",
            Key::Seq => "seq",
        }
    }
}

/// Intended-schedule truth for one run, over the completed requests in index
/// order (indexed by `i` when none was abandoned).
pub struct Truth {
    /// `sent_ns_i - (origin + i * PERIOD)`.
    pub truth_ns: Vec<i64>,
    /// `arrival_ns_i - (origin + i * PERIOD)`: how late request `i` reached
    /// phantom after its intended send time, its send lateness plus the trip.
    pub lateness_ns: Vec<i64>,
    /// Lines with `"completed": false`: the client went away first. They keep
    /// their index but have no sent_ns, so they are in no truth value.
    pub abandoned: usize,
    /// How far the origin is before request 0's arrival (>= 0): what aligning
    /// on request 0 would have added to every truth value's error.
    pub origin_shift_ns: i64,
    /// Requests phantom stalled.
    pub stalled: usize,
}

/// Truth for every completed request in phantom's log, indexed by `key`.
/// Panics if an index in 0..=max is missing or logged twice; an abandoned
/// line counts as present.
pub fn truth_from_log(text: &str, key: Key) -> Truth {
    let mut lines = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap());
    assert_eq!(lines.next().unwrap()["type"], "manifest");
    // Some((arrival_ns, sent_ns)) by index, or None if abandoned.
    let mut by_index: Vec<Option<Option<(u64, u64)>>> = Vec::new();
    let (mut stalled, mut abandoned) = (0, 0);
    for r in lines {
        let i = r[key.field()]
            .as_u64()
            .unwrap_or_else(|| panic!("no {}: {r}", key.field())) as usize;
        if by_index.len() <= i {
            by_index.resize(i + 1, None);
        }
        let times = if r["completed"] == false {
            abandoned += 1;
            None
        } else {
            Some((r["arrival_ns"].as_u64().unwrap(), r["sent_ns"].as_u64().unwrap()))
        };
        assert!(by_index[i].replace(times).is_none(), "{} {i} logged twice", key.field());
        if r["stall_wait_ns"].as_u64().unwrap() > 0 {
            stalled += 1;
        }
    }
    // (i, arrival_ns, sent_ns) for the completed requests.
    let completed: Vec<(i64, u64, u64)> = by_index
        .into_iter()
        .enumerate()
        .filter_map(|(i, t)| {
            t.unwrap_or_else(|| panic!("{} {i} not in the log", key.field()))
                .map(|(arrival, sent)| (i as i64, arrival, sent))
        })
        .collect();
    // Why the least-lagged request: arrival_ns_i - i * PERIOD is the schedule
    // start on phantom's clock PLUS request i's own delay (scheduler lag,
    // client send, loopback, header parsing), and that delay is never
    // negative. Any single request, request 0 included, carries its own delay
    // into every truth value; the minimum over all requests carries only the
    // smallest one. Stalls do not bias it: phantom applies them after arrival,
    // and late sends only raise arrivals.
    let period = period_ns();
    let origin = completed
        .iter()
        .map(|&(i, arrival, _)| arrival as i64 - i * period)
        .min()
        .unwrap();
    let intended = |i: i64| origin + i * period;
    Truth {
        truth_ns: completed
            .iter()
            .map(|&(i, _, sent)| sent as i64 - intended(i))
            .collect(),
        lateness_ns: completed
            .iter()
            .map(|&(i, arrival, _)| arrival as i64 - intended(i))
            .collect(),
        abandoned,
        // Request 0's, or the first completed request's if 0 was abandoned.
        origin_shift_ns: completed[0].1 as i64 - intended(completed[0].0),
        stalled,
    }
}

/// Nearest rank, the definition tailbench's histogram uses (without its
/// rounding up to the bucket, which is under 0.1%).
pub fn truth_at(sorted: &[i64], p: Pct) -> f64 {
    let n = sorted.len();
    let rank = match p.q {
        Some(q) => ((q * n as f64).ceil() as usize).max(1),
        None => n,
    };
    sorted[rank - 1] as f64
}

pub fn sorted(truth_ns: &[i64]) -> Vec<i64> {
    let mut sorted = truth_ns.to_vec();
    sorted.sort_unstable();
    sorted
}

pub fn tailbench_at(result: &RunResult, p: Pct) -> f64 {
    let h = &result.latency.hist;
    let us = match p.q {
        Some(q) => h.value_at_quantile(q),
        None => h.max(),
    };
    us as f64 * 1e3
}
