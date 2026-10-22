//! Audit of GuideLLM's streaming metrics (TTFT, ITL, end-to-end latency)
//! against phantom's per-token ground truth. This is a report, not a
//! pass/fail gate for GuideLLM: the only assertions are that GuideLLM exited
//! 0, its JSON parsed, and phantom logged at least 20 requests.
//!
//! GuideLLM is an external Python tool, installed outside the repo; this test
//! runs the binary named by GUIDELLM_BIN. The scenarios are #[ignore]; run
//! them with
//!
//! ```sh
//! GUIDELLM_BIN=~/audit-tools/guidellm-venv/bin/guidellm \
//!     cargo test --release -p tailbench-phantom --test audit_guidellm -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Truth comes from phantom's log through `common` (stream_lines,
//! stream_schedule_truth): server-side truth from phantom's own timestamps,
//! and, for the constant profile, intended-schedule truth keyed by seq with
//! the same origin formula as the other audits.
//!
//! What GuideLLM 0.8.1 measures (read from its source; see the README):
//! TTFT runs from the request's header write to the first SSE line with
//! non-empty content; ITL is per request, (last token line - first token
//! line) / (output_tokens - 1), and its distribution weights each request by
//! output_tokens - 1; request_latency (seconds) runs from header write to the
//! end of the stream, and request_scheduled_latency (seconds) from the
//! request's targeted start. Percentiles are taken from a weighted CDF with
//! numpy's searchsorted, which `common::guidellm_percentile` replicates.
//!
//! Each line beginning `CELL` is one table value, for aggregating runs.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use common::{
    StreamLog, StreamScheduleTruth, guidellm_percentile, nearest_rank, start_phantom_script, stream_lines,
    stream_schedule_truth,
};
use phantom::{RoleChunk, Script, ServiceSpec, Stall, StreamSpec};
use serde_json::{Value, json};

const GUIDELLM_ENV: &str = "GUIDELLM_BIN";
const DURATION_S: u64 = 30;
const RATE: f64 = 5.0;
/// 1 / RATE: the constant profile's intended gap between sends.
const PERIOD_NS: i64 = 200_000_000;
const TTFT_MS: u64 = 100;
const ITL_MS: u64 = 20;
/// phantom's ceiling; the request's max_completion_tokens (GuideLLM's
/// output_tokens) wins.
const PHANTOM_OUTPUT_TOKENS: u64 = 64;
const GUIDELLM_OUTPUT_TOKENS: u64 = 32;
const PROMPT_TOKENS: u64 = 16;
const GUIDELLM_SEED: u64 = 42;
const STALL: Stall = Stall {
    start_ms: 10_000,
    len_ms: 2_000,
};
const MIN_LOGGED: usize = 20;
const NS_PER_MS: f64 = 1e6;
const MS_PER_S: f64 = 1e3;

/// The percentiles printed: GuideLLM's own keys, and `None` for its max.
const PCTS: [(&str, Option<f64>); 4] = [
    ("p50", Some(0.50)),
    ("p90", Some(0.90)),
    ("p99", Some(0.99)),
    ("max", None),
];

/// The GuideLLM binary from `var` (GUIDELLM_BIN), or why it cannot be used.
fn resolve_guidellm_bin(var: Option<OsString>) -> Result<PathBuf, String> {
    let path = PathBuf::from(var.ok_or_else(|| {
        format!(
            "{GUIDELLM_ENV} is not set. Install GuideLLM outside the repo (see phantom/README.md, \"Audit: \
             GuideLLM\") and set {GUIDELLM_ENV} to its guidellm binary."
        )
    })?);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{GUIDELLM_ENV}={} is not a file", path.display()))
    }
}

fn guidellm_bin() -> PathBuf {
    resolve_guidellm_bin(std::env::var_os(GUIDELLM_ENV)).unwrap_or_else(|why| panic!("{why}"))
}

#[derive(Clone, Copy, PartialEq)]
enum Profile {
    Constant,
    Synchronous,
}

struct Scenario {
    id: &'static str,
    title: &'static str,
    profile: Profile,
    stall: Option<Stall>,
    role_chunk: RoleChunk,
}

/// What one scenario produced.
struct Run {
    json: Value,
    log: StreamLog,
    /// Intended-schedule truth; constant profile only.
    schedule: Option<StreamScheduleTruth>,
}

fn guidellm_args(base: &str, profile: Profile, json_path: &Path) -> Vec<String> {
    // GuideLLM's defaults: its health check (GET /health) and its own token
    // limit (max_completion_tokens on chat), both of which phantom serves.
    let backend = json!({
        "kind": "openai_http",
        "target": base,
        "model": "phantom",
    });
    let profile = match profile {
        Profile::Constant => format!("kind=constant,rate={RATE}"),
        Profile::Synchronous => "kind=synchronous".to_owned(),
    };
    [
        "run",
        "--backend",
        &backend.to_string(),
        "--profile",
        &profile,
        "--constraint",
        &format!("kind=max_duration,seconds={DURATION_S}"),
        "--data",
        &format!("kind=synthetic_text,prompt_tokens={PROMPT_TOKENS},output_tokens={GUIDELLM_OUTPUT_TOKENS}"),
        "--tokenizer",
        "kind=huggingface_auto,model=gpt2",
        "--seed",
        &format!("kind=static,value={GUIDELLM_SEED}"),
        "--output",
        &format!("kind=json,path={}", json_path.display()),
        "--disable-console-interactive",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Runs GuideLLM against a fresh phantom (TTFT 100 ms, ITL 20 ms) and returns
/// its JSON and phantom's log, after the only assertions this audit makes.
fn run_scenario(s: &Scenario) -> Run {
    let bin = guidellm_bin();
    let script = Script::new(common::SEED, common::SERVICE, s.stall.into_iter().collect())
        .unwrap()
        .with_stream(StreamSpec {
            ttft: Some(ServiceSpec::Fixed { ms: TTFT_MS }),
            itl: ServiceSpec::Fixed { ms: ITL_MS },
            output_tokens: PHANTOM_OUTPUT_TOKENS,
        })
        .unwrap()
        .with_role_chunk(s.role_chunk);
    let server = start_phantom_script(script);
    let base = format!("http://{}", server.phantom.local_addr());
    // GuideLLM runs in a temporary directory, so nothing it writes lands in the repo.
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("guidellm.json");
    let args = guidellm_args(&base, s.profile, &json_path);
    // Blocks this thread only: phantom serves from its own runtime's workers.
    let out = Command::new(&bin).args(&args).current_dir(dir.path()).output().unwrap();
    let text = server.finish();

    println!("\n== {}. {} ==", s.id, s.title);
    println!("{} {}", bin.display(), shell_words(&args));
    assert!(
        out.status.success(),
        "GuideLLM exited with {}:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_str(&std::fs::read_to_string(&json_path).expect("GuideLLM wrote its JSON"))
        .expect("GuideLLM's JSON parses");
    let log = stream_lines(&text);
    assert!(
        log.completed.len() + log.abandoned >= MIN_LOGGED,
        "phantom logged {} requests",
        log.completed.len() + log.abandoned
    );
    let schedule = (s.profile == Profile::Constant).then(|| stream_schedule_truth(&log, PERIOD_NS));
    Run { json, log, schedule }
}

fn shell_words(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(['{', ' ', '"']) {
                format!("'{a}'")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// GuideLLM's one benchmark in the JSON.
fn benchmark(json: &Value) -> &Value {
    let benchmarks = json["benchmarks"].as_array().expect("benchmarks array");
    assert_eq!(benchmarks.len(), 1, "one benchmark per run");
    &benchmarks[0]
}

/// GuideLLM's value of `metric` over successful requests at `pct`, in ms;
/// None if the metric is absent (request_scheduled_latency off the constant
/// profile). `scale` converts the metric's unit to ms.
fn guidellm_ms(json: &Value, metric: &str, pct: (&str, Option<f64>), scale: f64) -> Option<f64> {
    let summary = &benchmark(json)["metrics"][metric]["successful"];
    if summary.is_null() {
        return None;
    }
    let value = match pct {
        (_, None) => &summary["max"],
        (key, Some(_)) => &summary["percentiles"][key],
    };
    Some(value.as_f64().unwrap_or_else(|| panic!("{metric} {} missing", pct.0)) * scale)
}

/// A truth column: label and (value, weight) samples in ms.
struct Truth<'a> {
    label: &'a str,
    samples: Vec<(f64, f64)>,
}

impl Truth<'_> {
    fn nearest_rank(&self, q: Option<f64>) -> f64 {
        nearest_rank(&self.samples.iter().map(|&(v, _)| v).collect::<Vec<_>>(), q)
    }

    fn at_guidellm(&self, q: Option<f64>) -> f64 {
        guidellm_percentile(&self.samples, q)
    }
}

fn unweighted(values: impl IntoIterator<Item = f64>) -> Vec<(f64, f64)> {
    values.into_iter().map(|v| (v, 1.0)).collect()
}

/// Prints one metric: GuideLLM's value, then for each truth its value at
/// nearest rank and at GuideLLM's method, with GuideLLM − truth.
fn table(
    id: &str,
    name: &str,
    guidellm_label: &str,
    guidellm: impl Fn((&str, Option<f64>)) -> Option<f64>,
    truths: &[Truth],
) {
    println!("{name} (ms; delta = GuideLLM - truth)");
    let mut header = format!("{:<5} | {:>10}", "pct", guidellm_label);
    for t in truths {
        header += &format!(
            " | {:>22} {:>8} | {:>10} {:>8}",
            format!("{} NR", t.label),
            "delta",
            "GL-method",
            "delta"
        );
    }
    println!("{header}");
    for pct in PCTS {
        let Some(g) = guidellm(pct) else {
            println!("{:<5} | {:>10}", pct.0, "not reported");
            continue;
        };
        println!("CELL\t{id}\t{name}\t{}\tguidellm\t{g:.3}", pct.0);
        let mut row = format!("{:<5} | {:>10.3}", pct.0, g);
        for t in truths {
            let (nr, gl) = (t.nearest_rank(pct.1), t.at_guidellm(pct.1));
            println!("CELL\t{id}\t{name}\t{}\t{} NR\t{nr:.3}\t{:.3}", pct.0, t.label, g - nr);
            println!("CELL\t{id}\t{name}\t{}\t{} GL\t{gl:.3}\t{:.3}", pct.0, t.label, g - gl);
            row += &format!(" | {:>22.3} {:>+8.3} | {:>10.3} {:>+8.3}", nr, g - nr, gl, g - gl);
        }
        println!("{row}");
    }
}

fn ms(ns: impl Into<f64>) -> f64 {
    ns.into() / NS_PER_MS
}

/// Prints the scenario's report. `reference` is the intended-schedule truth
/// of another run, for a profile with no schedule of its own (synchronous).
fn report(id: &str, run: &Run, reference: Option<(&str, &StreamScheduleTruth)>) {
    let b = benchmark(&run.json);
    let totals = &b["metrics"]["request_totals"];
    println!(
        "requests: GuideLLM successful {}, incomplete {}, errored {}, total {}; phantom completed {}, \
         \"completed\": false {}",
        totals["successful"],
        totals["incomplete"],
        totals["errored"],
        totals["total"],
        run.log.completed.len(),
        run.log.abandoned
    );
    let output = &b["metrics"]["output_token_count"]["successful"];
    let tokens: Vec<usize> = run.log.completed.iter().map(|l| l.token_ns.len()).collect();
    println!(
        "output tokens: GuideLLM mean {} (min {}, max {}); phantom tokens per completed line {}..={}",
        output["mean"],
        output["min"],
        output["max"],
        tokens.iter().min().unwrap(),
        tokens.iter().max().unwrap()
    );
    println!(
        "CELL\t{id}\trequests\tcount\tguidellm successful/incomplete\t{}/{}\tphantom completed/false\t{}/{}",
        totals["successful"],
        totals["incomplete"],
        run.log.completed.len(),
        run.log.abandoned
    );

    let lines = &run.log.completed;
    let server_ttft = Truth {
        label: "server",
        samples: unweighted(lines.iter().map(|l| ms(l.ttft_ns() as f64))),
    };
    let server_e2e = Truth {
        label: "server",
        samples: unweighted(lines.iter().map(|l| ms(l.e2e_ns() as f64))),
    };
    // Weighted like GuideLLM's: each request's mean by its number of gaps.
    let mean_itl = Truth {
        label: "per-request mean",
        samples: lines
            .iter()
            .filter_map(|l| l.mean_itl_ns().map(|m| (ms(m), (l.token_ns.len() - 1) as f64)))
            .collect(),
    };
    let pooled_itl = Truth {
        label: "pooled gaps",
        samples: unweighted(lines.iter().flat_map(|l| l.gaps_ns()).map(|g| ms(g as f64))),
    };
    let (schedule, label) = match (&run.schedule, reference) {
        (Some(s), _) => (Some(s), "intended"),
        (None, Some((label, s))) => (Some(s), label),
        (None, None) => (None, ""),
    };
    let intended = |values: &dyn Fn(&StreamScheduleTruth) -> &Vec<i64>| {
        schedule.map(|s| Truth {
            label,
            samples: unweighted(values(s).iter().map(|&v| ms(v as f64))),
        })
    };
    let intended_ttft = intended(&|s| &s.ttft_ns);
    let intended_e2e = intended(&|s| &s.e2e_ns);

    let json = &run.json;
    let mut ttft_truths = vec![server_ttft];
    ttft_truths.extend(intended_ttft);
    table(
        id,
        "TTFT",
        "GuideLLM",
        |p| guidellm_ms(json, "time_to_first_token_ms", p, 1.0),
        &ttft_truths,
    );
    table(
        id,
        "ITL",
        "GuideLLM",
        |p| guidellm_ms(json, "inter_token_latency_ms", p, 1.0),
        &[mean_itl, pooled_itl],
    );
    // A run with its own schedule compares GuideLLM's scheduled latency with
    // its intended-schedule truth. Without one (synchronous), GuideLLM
    // reports no scheduled latency, so its request_latency is set against the
    // reference run's intended-schedule truth instead.
    let (own_intended_e2e, reference_e2e) = match run.schedule {
        Some(_) => (intended_e2e, None),
        None => (None, intended_e2e),
    };
    let mut e2e_truths = vec![server_e2e];
    e2e_truths.extend(reference_e2e);
    table(
        id,
        "E2E",
        "GuideLLM",
        |p| guidellm_ms(json, "request_latency", p, MS_PER_S),
        &e2e_truths,
    );
    if let Some(intended_e2e) = own_intended_e2e {
        table(
            id,
            "E2E scheduled",
            "GuideLLM",
            |p| guidellm_ms(json, "request_scheduled_latency", p, MS_PER_S),
            &[intended_e2e],
        );
    }
    if let Some(s) = &run.schedule {
        let lateness = unweighted(s.lateness_ns.iter().map(|&v| ms(v as f64)));
        let values: Vec<f64> = lateness.iter().map(|&(v, _)| v).collect();
        let late = s.lateness_ns.iter().filter(|&&l| l > PERIOD_NS).count();
        println!(
            "send lateness (arrival - intended): p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms; {} of {} later than one \
             period",
            nearest_rank(&values, Some(0.5)),
            nearest_rank(&values, Some(0.99)),
            nearest_rank(&values, None),
            late,
            values.len()
        );
        for (key, q) in [("p50", Some(0.5)), ("p99", Some(0.99)), ("max", None)] {
            println!("CELL\t{id}\tlateness\t{key}\tphantom\t{:.3}", nearest_rank(&values, q));
        }
    }
}

const G1: Scenario = Scenario {
    id: "G1",
    title: "constant 5 req/s, no stall",
    profile: Profile::Constant,
    stall: None,
    role_chunk: RoleChunk::WithFirstToken,
};
const G2: Scenario = Scenario {
    id: "G2",
    title: "constant 5 req/s, stall 10000:2000",
    profile: Profile::Constant,
    stall: Some(STALL),
    role_chunk: RoleChunk::WithFirstToken,
};
const G3: Scenario = Scenario {
    id: "G3",
    title: "synchronous, stall 10000:2000 (closed loop)",
    profile: Profile::Synchronous,
    stall: Some(STALL),
    role_chunk: RoleChunk::WithFirstToken,
};
const G4: Scenario = Scenario {
    id: "G4",
    title: "constant 5 req/s, no stall, --role-chunk at-arrival",
    profile: Profile::Constant,
    stall: None,
    role_chunk: RoleChunk::AtArrival,
};

/// G2's run, shared with G3 as its cross-run reference: whichever needs it
/// first runs it.
fn g2() -> &'static Run {
    static G2_RUN: OnceLock<Run> = OnceLock::new();
    G2_RUN.get_or_init(|| run_scenario(&G2))
}

#[test]
#[ignore = "runs the external GuideLLM binary for 30 s"]
fn g1_constant_no_stall() {
    report("G1", &run_scenario(&G1), None);
}

#[test]
#[ignore = "runs the external GuideLLM binary for 30 s"]
fn g2_constant_stall() {
    report("G2", g2(), None);
}

#[test]
#[ignore = "runs the external GuideLLM binary for 30 s (and G2's, if not yet run)"]
fn g3_synchronous_stall() {
    let reference = g2().schedule.as_ref().expect("G2 is constant");
    report("G3", &run_scenario(&G3), Some(("G2 intended (cross-run)", reference)));
}

#[test]
#[ignore = "runs the external GuideLLM binary for 30 s"]
fn g4_constant_role_chunk_at_arrival() {
    let run = run_scenario(&G4);
    report("G4", &run, None);
    // Where the role chunk went out, against the first content token.
    let offsets: Vec<f64> = run
        .log
        .completed
        .iter()
        .map(|l| ms(l.role_chunk_ns.expect("chat line") as f64 - l.arrival_ns as f64))
        .collect();
    println!(
        "role chunk: handed off {:.3} ms (p50) after arrival; first content token {:.3} ms (p50) after arrival",
        nearest_rank(&offsets, Some(0.5)),
        nearest_rank(
            &run.log
                .completed
                .iter()
                .map(|l| ms(l.ttft_ns() as f64))
                .collect::<Vec<_>>(),
            Some(0.5)
        )
    );
}

// Checks of the helpers above, without GuideLLM.

#[test]
fn guidellm_bin_is_required() {
    let unset = resolve_guidellm_bin(None).unwrap_err();
    assert!(unset.contains(GUIDELLM_ENV) && unset.contains("not set"), "{unset}");
    let missing = resolve_guidellm_bin(Some("/nonexistent/guidellm".into())).unwrap_err();
    assert!(
        missing.contains("/nonexistent/guidellm") && missing.contains("not a file"),
        "{missing}"
    );
}

/// Expected values from GuideLLM 0.8.1's own DistributionSummary.from_values
/// (run in its venv). Where they differ from nearest rank it is by numpy's
/// float cumulative sum, reproduced here.
#[test]
fn guidellm_percentile_matches_guidellm() {
    let range = |n: u32| unweighted((1..=n).map(f64::from));
    let check = |values: &[(f64, f64)], expected: [f64; 4]| {
        let got = [0.5, 0.9, 0.99, 0.999].map(|q| guidellm_percentile(values, Some(q)));
        assert_eq!(got, expected, "{values:?}");
    };
    check(&range(150), [76.0, 135.0, 149.0, 150.0]);
    check(&range(10), [5.0, 10.0, 10.0, 10.0]);
    check(&range(1000), [500.0, 900.0, 990.0, 999.0]);
    check(&unweighted([5.0, 5.0, 1.0, 3.0, 3.0, 3.0, 9.0]), [3.0, 9.0, 9.0, 9.0]);
    check(
        &[(10.0, 31.0), (20.0, 31.0), (30.0, 62.0), (40.0, 1.0)],
        [30.0, 30.0, 30.0, 40.0],
    );
    assert_eq!(guidellm_percentile(&range(10), None), 10.0, "None is the max");
    // Nearest rank differs at n = 150, p50 and n = 10, p90.
    let plain = |n: u32| (1..=n).map(f64::from).collect::<Vec<_>>();
    assert_eq!(nearest_rank(&plain(150), Some(0.5)), 75.0);
    assert_eq!(nearest_rank(&plain(10), Some(0.9)), 9.0);
    assert_eq!(nearest_rank(&plain(10), None), 10.0);
}

/// Stream truth from a hand-written log: server-side values, abandoned lines
/// counted but kept out, and intended-schedule truth keyed by seq.
#[test]
fn stream_truth_from_a_known_log() {
    let log = [
        r#"{"type":"manifest"}"#,
        r#"{"type":"req","seq":0,"arrival_ns":1000,"sent_ns":9000,"stream":true,"token_ns":[5000,7000,8000],"role_chunk_ns":4900}"#,
        r#"{"type":"req","seq":1,"completed":false,"arrival_ns":2500,"abandoned_ns":6000,"stream":true,"token_ns":[5500]}"#,
        r#"{"type":"req","seq":2,"arrival_ns":2400,"sent_ns":20000,"stream":true,"token_ns":[12000,13000],"role_chunk_ns":2500}"#,
    ]
    .join("\n");
    let log = stream_lines(&log);
    assert_eq!(log.abandoned, 1);
    let seqs: Vec<u64> = log.completed.iter().map(|l| l.seq).collect();
    assert_eq!(seqs, [0, 2]);
    let first = &log.completed[0];
    assert_eq!(first.ttft_ns(), 4000);
    assert_eq!(first.gaps_ns(), [2000, 1000]);
    assert_eq!(first.mean_itl_ns(), Some(1500.0));
    assert_eq!(first.e2e_ns(), 8000);
    assert_eq!(first.role_chunk_ns, Some(4900));
    let second = &log.completed[1];
    assert_eq!(
        (second.ttft_ns(), second.mean_itl_ns(), second.e2e_ns()),
        (9600, Some(1000.0), 17600)
    );

    // Period 1000: arrival - seq * period is 1000 for seq 0 and 400 for
    // seq 2, so the origin is 400 (the abandoned seq 1 is not used).
    let s = stream_schedule_truth(&log, 1000);
    assert_eq!(s.origin_ns, 400);
    assert_eq!(s.ttft_ns, [5000 - 400, 12000 - 2400]);
    assert_eq!(s.e2e_ns, [9000 - 400, 20000 - 2400]);
    assert_eq!(s.lateness_ns, [1000 - 400, 0]);
}
