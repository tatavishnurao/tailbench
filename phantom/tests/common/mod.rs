//! Shared by the audits (audit.rs, audit_oha.rs, audit_guidellm.rs,
//! audit_vllm.rs): starting phantom, running
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
    start_phantom_script(Script::new(SEED, SERVICE, stalls).unwrap())
}

/// Starts phantom with `script`.
pub fn start_phantom_script(script: Script) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("gt.jsonl");
    let rt = runtime("phantom");
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
    let period = period_ns();
    let origin = schedule_origin(completed.iter().map(|&(i, arrival, _)| (i, arrival)), period);
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

/// The schedule start on phantom's clock: `phantom::report::schedule_origin`,
/// the minimum over requests of `arrival_ns_i - i * period_ns` (see there
/// for why the least-lagged request).
pub fn schedule_origin(arrivals: impl IntoIterator<Item = (i64, u64)>, period_ns: i64) -> i64 {
    phantom::report::schedule_origin(arrivals, period_ns).expect("at least one request")
}

/// Nearest rank, the definition tailbench's histogram uses (without its
/// rounding up to the bucket, which is under 0.1%), and `phantom report`'s.
pub fn truth_at(sorted: &[i64], p: Pct) -> f64 {
    phantom::report::nearest_rank(sorted, p.q) as f64
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

// Streams (audit_guidellm.rs): per-token truth from phantom's log.

/// One completed streamed request in phantom's log; times in ns since t0.
pub struct StreamLine {
    pub seq: u64,
    pub arrival_ns: u64,
    pub token_ns: Vec<u64>,
    pub sent_ns: u64,
    /// Chat only.
    pub role_chunk_ns: Option<u64>,
    /// The request's `x-request-id`, when phantom logged one (a u64).
    pub client_id: Option<u64>,
}

impl StreamLine {
    /// Server TTFT: from handler entry to token 0's hand-off.
    pub fn ttft_ns(&self) -> u64 {
        self.token_ns[0] - self.arrival_ns
    }

    /// Every inter-token gap: consecutive differences of token_ns.
    pub fn gaps_ns(&self) -> Vec<u64> {
        self.token_ns.windows(2).map(|w| w[1] - w[0]).collect()
    }

    /// The request's mean gap, (last - first) / (tokens - 1); None for one token.
    pub fn mean_itl_ns(&self) -> Option<f64> {
        let n = self.token_ns.len();
        (n > 1).then(|| (self.token_ns[n - 1] - self.token_ns[0]) as f64 / (n - 1) as f64)
    }

    /// Server end-to-end: from handler entry to the [DONE] hand-off.
    pub fn e2e_ns(&self) -> u64 {
        self.sent_ns - self.arrival_ns
    }
}

/// phantom's streamed lines: the completed ones by seq, and how many were
/// abandoned (`"completed": false`, kept out of every truth value).
pub struct StreamLog {
    pub completed: Vec<StreamLine>,
    pub abandoned: usize,
}

impl StreamLog {
    /// Removes and returns the completed lines without a `client_id`: requests
    /// the load generator sent without its per-request id, such as vLLM's
    /// initial test and warm-up requests.
    pub fn take_untagged(&mut self) -> Vec<StreamLine> {
        let (tagged, untagged) = std::mem::take(&mut self.completed)
            .into_iter()
            .partition(|l| l.client_id.is_some());
        self.completed = tagged;
        untagged
    }
}

/// Parses a log whose every request was streamed; panics on any other line.
pub fn stream_lines(text: &str) -> StreamLog {
    let mut lines = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap());
    assert_eq!(lines.next().unwrap()["type"], "manifest");
    let u64_of = |r: &Value, key: &str| r[key].as_u64().unwrap_or_else(|| panic!("no {key}: {r}"));
    let (mut completed, mut abandoned) = (Vec::new(), 0);
    for r in lines {
        assert_eq!(r["stream"], true, "not a streamed line: {r}");
        if r["completed"] == false {
            abandoned += 1;
            continue;
        }
        completed.push(StreamLine {
            seq: u64_of(&r, "seq"),
            arrival_ns: u64_of(&r, "arrival_ns"),
            token_ns: r["token_ns"]
                .as_array()
                .unwrap_or_else(|| panic!("no token_ns: {r}"))
                .iter()
                .map(|v| v.as_u64().unwrap())
                .collect(),
            sent_ns: u64_of(&r, "sent_ns"),
            role_chunk_ns: r["role_chunk_ns"].as_u64(),
            client_id: r["client_id"].as_u64(),
        });
    }
    completed.sort_by_key(|l| l.seq);
    StreamLog { completed, abandoned }
}

/// Intended-schedule truth for streams, keyed by seq (send i is seq i; see
/// `Key::Seq` for the ordering assumption), one value per completed line in
/// seq order. Request i's intended send time is `origin + i * period`.
pub struct StreamScheduleTruth {
    pub origin_ns: i64,
    /// `token_ns[0] - intended`.
    pub ttft_ns: Vec<i64>,
    /// `sent_ns - intended`.
    pub e2e_ns: Vec<i64>,
    /// `arrival_ns - intended`: send lateness plus the trip.
    pub lateness_ns: Vec<i64>,
}

pub fn stream_schedule_truth(log: &StreamLog, period_ns: i64) -> StreamScheduleTruth {
    let index = |l: &StreamLine| l.seq as i64;
    let origin = schedule_origin(log.completed.iter().map(|l| (index(l), l.arrival_ns)), period_ns);
    let since_intended = |l: &StreamLine, t: u64| t as i64 - (origin + index(l) * period_ns);
    let each = |f: &dyn Fn(&StreamLine) -> i64| log.completed.iter().map(f).collect();
    StreamScheduleTruth {
        origin_ns: origin,
        ttft_ns: each(&|l| since_intended(l, l.token_ns[0])),
        e2e_ns: each(&|l| since_intended(l, l.sent_ns)),
        lateness_ns: each(&|l| since_intended(l, l.arrival_ns)),
    }
}

/// Nearest rank over unsorted `values` (`ceil(q * n)`-th smallest); `None` is
/// the maximum.
pub fn nearest_rank(values: &[f64], q: Option<f64>) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let rank = q.map_or(n, |q| ((q * n as f64).ceil() as usize).max(1));
    sorted[rank - 1]
}

/// GuideLLM 0.8.1's percentile of (value, weight) samples
/// (`DistributionSummary.from_values` then `Percentiles.from_pdf`, in
/// guidellm/schemas/base/statistics.py): sort by value, merge equal values by
/// summing their weights, normalise the weights to probabilities, take their
/// running sum, and return the first value whose running sum is >= q (numpy
/// `searchsorted(..., side="left")`). The running sum is accumulated one f64
/// addition at a time, as numpy's `cumsum` does, so it can land one rank
/// above nearest rank. `None` is the maximum, GuideLLM's `max`.
pub fn guidellm_percentile(samples: &[(f64, f64)], q: Option<f64>) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (value, weight) in sorted {
        match merged.last_mut() {
            Some(last) if last.0 == value => last.1 += weight,
            _ => merged.push((value, weight)),
        }
    }
    merged.retain(|&(_, weight)| weight > 0.0);
    let Some(q) = q else {
        return merged.last().expect("at least one sample").0;
    };
    // Integer weights in these audits, so the total is exact however numpy
    // orders its sum.
    let total: f64 = merged.iter().map(|&(_, weight)| weight).sum();
    let mut cdf = 0.0;
    for &(value, weight) in &merged {
        cdf += weight / total;
        if cdf >= q {
            return value;
        }
    }
    // numpy would index past the end here; it cannot happen for q <= 0.999.
    merged.last().unwrap().0
}

/// numpy's `np.percentile(values, p)` with its default method, "linear", as
/// vLLM's benchmark calls it (numpy/lib/_function_base_impl.py, `_quantile`
/// and `_lerp`): sort, take the virtual index `(n - 1) * p / 100`, and
/// interpolate between its two neighbours. Like numpy's `_lerp`, it
/// interpolates down from the upper neighbour when the weight is >= 0.5, so
/// the result matches numpy's to the bit. p = 100 is the maximum.
pub fn numpy_percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let last = sorted.len().checked_sub(1).expect("at least one value");
    let index = last as f64 * (p / 100.0);
    if index >= last as f64 {
        return sorted[last];
    }
    let below = index.floor();
    let (a, b) = (sorted[below as usize], sorted[below as usize + 1]);
    let (t, diff) = (index - below, b - a);
    if t >= 0.5 { b - diff * (1.0 - t) } else { a + diff * t }
}
