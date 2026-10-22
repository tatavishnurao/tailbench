//! Audit of `vllm bench serve`'s streaming metrics (TTFT, TPOT, ITL and
//! end-to-end latency) against phantom's per-token ground truth. This is a
//! report, not a pass/fail gate for vLLM: the only assertions are that vLLM
//! exited 0, its result JSON parsed, and phantom logged at least 20 requests.
//!
//! vLLM is an external Python tool, installed outside the repo; this test
//! runs the binary named by VLLM_BIN. The scenarios are #[ignore]; run them
//! with
//!
//! ```sh
//! VLLM_BIN=~/audit-tools/vllm-venv/bin/vllm \
//!     cargo test --release -p tailbench-phantom --test audit_vllm -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Truth comes from phantom's log through `common` (stream_lines,
//! stream_schedule_truth): server-side truth from phantom's own timestamps,
//! and, for the scenarios sent at constant spacing, intended-schedule truth
//! with the same origin formula as the other audits, keyed by seq (V1, V2)
//! or, behind `--max-concurrency`, by client_id (V5).
//!
//! What vLLM 0.32.0 measures (read from its source; see the README): TTFT
//! runs from just before the request is posted to the first SSE message with
//! a non-empty `choices`; ITL is every gap between such messages, pooled over
//! requests; TPOT is per request, (latency - TTFT) / (output tokens - 1);
//! latency (E2EL) ends at the last such message. Output tokens come from the
//! server's usage only on a chunk without `choices`, which phantom sends
//! because vLLM asks for it; otherwise vLLM re-tokenizes the text, so
//! phantom's token text here is one gpt2 token either way. Percentiles are
//! numpy's `np.percentile` (linear interpolation), which
//! `common::numpy_percentile` replicates.
//!
//! Each line beginning `CELL` is one table value, for aggregating runs.

mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use common::{
    StreamLine, StreamLog, StreamScheduleTruth, nearest_rank, numpy_percentile, schedule_origin, start_phantom_script,
    stream_lines, stream_schedule_truth,
};
use phantom::{Script, ServiceSpec, Stall, StreamSpec};
use serde_json::Value;

const VLLM_ENV: &str = "VLLM_BIN";
/// At 5 requests/s, 30 s.
const NUM_PROMPTS: usize = 150;
/// 1 / 5 requests/s: the intended gap between sends at constant spacing.
const PERIOD_NS: i64 = 200_000_000;
const TTFT_MS: u64 = 100;
const ITL_MS: u64 = 20;
/// phantom's ceiling; the request's max_tokens (vLLM's output length) wins.
const PHANTOM_OUTPUT_TOKENS: u64 = 64;
const VLLM_OUTPUT_TOKENS: u64 = 32;
const PROMPT_TOKENS: u64 = 16;
/// Exactly one gpt2 token (id 262), so a re-tokenized count would also be one
/// per chunk; phantom's default " tok" is two.
const TOKEN_TEXT: &str = " the";
const TOKENIZER: &str = "gpt2";
const VLLM_SEED: u64 = 42;
const STALL: Stall = Stall {
    start_ms: 10_000,
    len_ms: 2_000,
};
const MIN_LOGGED: usize = 20;
const NS_PER_MS: f64 = 1e6;

/// The percentiles printed: vLLM's result keys (`--metric-percentiles`) and
/// the percentile; p100 is numpy's maximum.
const PCTS: [(&str, f64); 4] = [("p50", 50.0), ("p90", 90.0), ("p99", 99.0), ("p100", 100.0)];

/// The vLLM binary from `var` (VLLM_BIN), or why it cannot be used.
fn resolve_vllm_bin(var: Option<OsString>) -> Result<PathBuf, String> {
    let path = PathBuf::from(var.ok_or_else(|| {
        format!(
            "{VLLM_ENV} is not set. Install vLLM outside the repo (see phantom/README.md, \"Audit: vLLM bench \
             serve\") and set {VLLM_ENV} to its vllm binary."
        )
    })?);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{VLLM_ENV}={} is not a file", path.display()))
    }
}

fn vllm_bin() -> PathBuf {
    resolve_vllm_bin(std::env::var_os(VLLM_ENV)).unwrap_or_else(|why| panic!("{why}"))
}

struct Scenario {
    id: &'static str,
    title: &'static str,
    /// `--request-rate`.
    rate: &'static str,
    /// `--burstiness`, if set.
    burstiness: Option<&'static str>,
    /// `--max-concurrency`, if set.
    max_concurrency: Option<u32>,
    stall: Option<Stall>,
    schedule: Schedule,
}

/// Whether intended-schedule truth applies, and how requests are matched to
/// their intended send times.
#[derive(Clone, Copy, PartialEq)]
enum Schedule {
    /// No schedule phantom knows: Poisson times drawn inside vLLM, or a
    /// closed loop.
    None,
    /// Constant spacing; request i is phantom's seq i.
    BySeq,
    /// Constant spacing; request i is client_id i (vLLM's request index).
    /// Behind `--max-concurrency`, requests can reach phantom out of index
    /// order, so seq is not used.
    ByClientId,
}

/// What one scenario produced.
struct Run {
    json: Value,
    log: StreamLog,
    /// Completed lines without a client_id, taken out of `log`.
    untagged: usize,
    /// Intended-schedule truth; constant spacing only.
    schedule: Option<StreamScheduleTruth>,
}

fn vllm_args(base: &str, s: &Scenario, result_dir: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "bench",
        "serve",
        "--backend",
        "openai",
        "--base-url",
        base,
        "--endpoint",
        "/v1/completions",
        "--model",
        "phantom",
        "--tokenizer",
        TOKENIZER,
        "--dataset-name",
        "random",
        "--random-input-len",
        &PROMPT_TOKENS.to_string(),
        "--random-output-len",
        &VLLM_OUTPUT_TOKENS.to_string(),
        "--ignore-eos",
        "--num-prompts",
        &NUM_PROMPTS.to_string(),
        "--request-rate",
        s.rate,
        "--seed",
        &VLLM_SEED.to_string(),
        // Request i's x-request-id is "i", which phantom logs as client_id.
        "--request-id-prefix",
        "",
        "--percentile-metrics",
        "ttft,tpot,itl,e2el",
        "--metric-percentiles",
        "50,90,99,100",
        "--save-result",
        "--save-detailed",
        "--result-dir",
        result_dir,
        "--result-filename",
        "vllm.json",
        "--disable-tqdm",
    ]
    .map(str::to_owned)
    .to_vec();
    if let Some(b) = s.burstiness {
        args.extend(["--burstiness".to_owned(), b.to_owned()]);
    }
    if let Some(c) = s.max_concurrency {
        args.extend(["--max-concurrency".to_owned(), c.to_string()]);
    }
    if s.reports_queue() {
        // vLLM reports these two only with --max-concurrency at a finite rate.
        let metrics = args.iter().position(|a| a == "--percentile-metrics").unwrap() + 1;
        args[metrics] = "ttft,tpot,itl,e2el,client_queue_time,e2el_including_client_queue".to_owned();
    }
    args
}

impl Scenario {
    /// Requests sent on a schedule can wait for a `--max-concurrency` slot:
    /// vLLM records that wait as client_queue_time.
    fn reports_queue(&self) -> bool {
        self.max_concurrency.is_some() && self.schedule != Schedule::None
    }
}

/// Runs vLLM against a fresh phantom (TTFT 100 ms, ITL 20 ms, token text
/// " the") and returns its result JSON and phantom's log, after the only
/// assertions this audit makes.
fn run_scenario(s: &Scenario) -> Run {
    let bin = vllm_bin();
    let script = Script::new(common::SEED, common::SERVICE, s.stall.into_iter().collect())
        .unwrap()
        .with_stream(StreamSpec {
            ttft: Some(ServiceSpec::Fixed { ms: TTFT_MS }),
            itl: ServiceSpec::Fixed { ms: ITL_MS },
            output_tokens: PHANTOM_OUTPUT_TOKENS,
        })
        .unwrap()
        .with_token_text(TOKEN_TEXT)
        .unwrap();
    let server = start_phantom_script(script);
    let base = format!("http://{}", server.phantom.local_addr());
    // vLLM runs in a temporary directory, so nothing it writes lands in the repo.
    let dir = tempfile::tempdir().unwrap();
    let args = vllm_args(&base, s, &dir.path().display().to_string());
    // Blocks this thread only: phantom serves from its own runtime's workers.
    let out = Command::new(&bin).args(&args).current_dir(dir.path()).output().unwrap();
    let text = server.finish();

    println!("\n== {}. {} ==", s.id, s.title);
    println!("{} {}", bin.display(), shell_words(&args));
    assert!(
        out.status.success(),
        "vLLM exited with {}:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let json_path = dir.path().join("vllm.json");
    let json: Value = serde_json::from_str(&python_json(
        &std::fs::read_to_string(&json_path).expect("vLLM wrote its result"),
    ))
    .expect("vLLM's result parses");
    let mut log = stream_lines(&text);
    assert!(
        log.completed.len() + log.abandoned >= MIN_LOGGED,
        "phantom logged {} requests",
        log.completed.len() + log.abandoned
    );
    // vLLM's benchmark requests carry x-request-id; its initial test and
    // warm-up requests do not, so they are the lines without a client_id.
    let untagged = log.take_untagged().len();
    let schedule = match s.schedule {
        Schedule::None => None,
        Schedule::BySeq => Some(stream_schedule_truth(&log, PERIOD_NS)),
        Schedule::ByClientId => Some(schedule_by_client_id(&log)),
    };
    Run {
        json,
        log,
        untagged,
        schedule,
    }
}

/// `stream_schedule_truth` keyed by client_id instead of seq: the same origin
/// formula (`schedule_origin`), with request i intended at origin + i ×
/// period. Values are in `log.completed` order.
fn schedule_by_client_id(log: &StreamLog) -> StreamScheduleTruth {
    let index = |l: &StreamLine| l.client_id.expect("untagged lines are taken out") as i64;
    let origin = schedule_origin(log.completed.iter().map(|l| (index(l), l.arrival_ns)), PERIOD_NS);
    let since_intended = |l: &StreamLine, t: u64| t as i64 - (origin + index(l) * PERIOD_NS);
    let each = |f: &dyn Fn(&StreamLine) -> i64| log.completed.iter().map(f).collect();
    StreamScheduleTruth {
        origin_ns: origin,
        ttft_ns: each(&|l| since_intended(l, l.token_ns[0])),
        e2e_ns: each(&|l| since_intended(l, l.sent_ns)),
        lateness_ns: each(&|l| since_intended(l, l.arrival_ns)),
    }
}

/// vLLM writes its result with Python's json module, which writes an
/// infinite `--request-rate` or `--burstiness` as `Infinity`, a literal JSON
/// does not have. Python's three such constants are read as strings; the
/// generated texts in the result (" the" repeated) cannot contain them.
fn python_json(text: &str) -> String {
    text.replace(": -Infinity", ": \"-Infinity\"")
        .replace(": Infinity", ": \"Infinity\"")
        .replace(": NaN", ": \"NaN\"")
}

fn shell_words(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(['{', ' ', '"']) {
                format!("'{a}'")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// vLLM's reported `metric` (ttft, tpot, itl or e2el) at `pct`, in ms.
fn vllm_ms(json: &Value, metric: &str, pct: &str) -> f64 {
    let key = format!("{pct}_{metric}_ms");
    json[&key].as_f64().unwrap_or_else(|| panic!("{key} missing"))
}

/// vLLM's per-request lists in the detailed result.
fn vllm_list<'a>(json: &'a Value, key: &str) -> &'a Vec<Value> {
    json[key].as_array().unwrap_or_else(|| panic!("{key} missing"))
}

/// A truth column: label and values in ms.
struct Truth<'a> {
    label: &'a str,
    values: Vec<f64>,
}

/// Prints one metric: vLLM's value, then for each truth its value at nearest
/// rank and at numpy's method, with vLLM − truth.
fn table(id: &str, name: &str, metric: &str, json: &Value, truths: &[Truth]) {
    println!("{name} (ms; delta = vLLM - truth)");
    let mut header = format!("{:<5} | {:>10}", "pct", "vLLM");
    for t in truths {
        header += &format!(
            " | {:>22} {:>8} | {:>10} {:>8}",
            format!("{} NR", t.label),
            "delta",
            "np-method",
            "delta"
        );
    }
    println!("{header}");
    for (key, p) in PCTS {
        let v = vllm_ms(json, metric, key);
        println!("CELL\t{id}\t{name}\t{key}\tvllm\t{v:.3}");
        let mut row = format!("{key:<5} | {v:>10.3}");
        for t in truths {
            let (nr, np) = (nearest_rank(&t.values, Some(p / 100.0)), numpy_percentile(&t.values, p));
            println!("CELL\t{id}\t{name}\t{key}\t{} NR\t{nr:.3}\t{:.3}", t.label, v - nr);
            println!("CELL\t{id}\t{name}\t{key}\t{} NP\t{np:.3}\t{:.3}", t.label, v - np);
            row += &format!(" | {:>22.3} {:>+8.3} | {:>10.3} {:>+8.3}", nr, v - nr, np, v - np);
        }
        println!("{row}");
    }
}

fn ms(ns: impl Into<f64>) -> f64 {
    ns.into() / NS_PER_MS
}

fn range<T: Ord + Copy + std::fmt::Display>(values: impl IntoIterator<Item = T>) -> String {
    let values: Vec<T> = values.into_iter().collect();
    match (values.iter().min(), values.iter().max()) {
        (Some(lo), Some(hi)) if lo == hi => format!("{lo}"),
        (Some(lo), Some(hi)) => format!("{lo}..={hi}"),
        _ => "none".to_owned(),
    }
}

/// Prints the scenario's report.
fn report(id: &str, run: &Run) {
    let json = &run.json;
    let lines = &run.log.completed;
    // With no untagged lines before them, request i is seq i.
    let id_matches_seq = lines.iter().filter(|l| l.client_id == Some(l.seq)).count();
    println!(
        "requests: vLLM completed {}, failed {}; phantom completed {}, \"completed\": false {}, without a client_id \
         (initial test or warm-up) {}; client_id == seq on {} of {}",
        json["completed"],
        json["failed"],
        lines.len(),
        run.log.abandoned,
        run.untagged,
        id_matches_seq,
        lines.len()
    );
    println!(
        "CELL\t{id}\trequests\tcount\tvllm completed/failed\t{}/{}\tphantom completed/false/untagged\t{}/{}/{}",
        json["completed"],
        json["failed"],
        lines.len(),
        run.log.abandoned,
        run.untagged
    );
    let output_lens = vllm_list(json, "output_lens").iter().map(|v| v.as_u64().unwrap());
    println!(
        "output tokens: vLLM output_lens {}; phantom tokens per completed line {}",
        range(output_lens),
        range(lines.iter().map(|l| l.token_ns.len()))
    );
    let itls = vllm_list(json, "itls");
    let per_request = itls.iter().map(|r| r.as_array().unwrap().len());
    let vllm_gaps: usize = per_request.clone().sum();
    let truth_gaps: usize = lines.iter().map(|l| l.token_ns.len() - 1).sum();
    let last_itl: Vec<f64> = itls
        .iter()
        .filter_map(|r| r.as_array().unwrap().last().map(|v| v.as_f64().unwrap() * 1e3))
        .collect();
    println!(
        "ITL samples: vLLM {vllm_gaps} ({} per request), phantom gaps {truth_gaps}; vLLM's last ITL per request: \
         p50 {:.3} ms, max {:.3} ms",
        range(per_request),
        nearest_rank(&last_itl, Some(0.5)),
        nearest_rank(&last_itl, None)
    );
    println!("CELL\t{id}\tITL samples\tcount\tvllm\t{vllm_gaps}\tphantom gaps\t{truth_gaps}");

    let server = |values: Vec<f64>| Truth {
        label: "server",
        values,
    };
    let intended = |values: &dyn Fn(&StreamScheduleTruth) -> &Vec<i64>| {
        run.schedule.as_ref().map(|s| Truth {
            label: "intended",
            values: values(s).iter().map(|&v| ms(v as f64)).collect(),
        })
    };

    let mut ttft = vec![server(lines.iter().map(|l| ms(l.ttft_ns() as f64)).collect())];
    ttft.extend(intended(&|s| &s.ttft_ns));
    table(id, "TTFT", "ttft", json, &ttft);
    let mean_itl = Truth {
        label: "per-request mean",
        values: lines.iter().filter_map(|l| l.mean_itl_ns().map(ms)).collect(),
    };
    table(id, "TPOT", "tpot", json, &[mean_itl]);
    let pooled = Truth {
        label: "per-gap",
        values: lines.iter().flat_map(|l| l.gaps_ns()).map(|g| ms(g as f64)).collect(),
    };
    table(id, "ITL", "itl", json, &[pooled]);
    let mut e2e = vec![server(lines.iter().map(|l| ms(l.e2e_ns() as f64)).collect())];
    e2e.extend(intended(&|s| &s.e2e_ns));
    table(id, "E2E", "e2el", json, &e2e);

    if let Some(s) = &run.schedule {
        let values: Vec<f64> = s.lateness_ns.iter().map(|&v| ms(v as f64)).collect();
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

/// For a scheduled scenario behind `--max-concurrency`: vLLM's
/// client_queue_time, its queue-inclusive E2EL, and, per request, vLLM's
/// TTFT and E2EL plus its client_queue_time against intended truth.
fn report_queue(id: &str, run: &Run) {
    let json = &run.json;
    let lines = &run.log.completed;
    let schedule = run.schedule.as_ref().expect("a scheduled scenario");
    // vLLM's per-request lists are in request order: index i is client_id i.
    let seconds = |key: &str| -> Vec<f64> { vllm_list(json, key).iter().map(|v| v.as_f64().unwrap()).collect() };
    let (queue, ttfts, latencies) = (seconds("queue_times"), seconds("ttfts"), seconds("latencies"));
    let at = |list: &[f64], l: &StreamLine| list[l.client_id.unwrap() as usize] * 1e3;
    let intended = |values: &[i64]| Truth {
        label: "intended",
        values: values.iter().map(|&v| ms(v as f64)).collect(),
    };
    let lateness = Truth {
        label: "lateness",
        values: schedule.lateness_ns.iter().map(|&v| ms(v as f64)).collect(),
    };
    let queued = queue.iter().filter(|&&q| q * 1e3 >= 1.0).count();
    println!(
        "client_queue_time: {queued} of {} requests waited 1 ms or more",
        queue.len()
    );
    println!("CELL\t{id}\tqueued\tcount\tvllm\t{queued}");
    table(id, "queue", "client_queue_time", json, &[lateness]);
    table(
        id,
        "E2E incl. queue",
        "e2el_including_client_queue",
        json,
        &[intended(&schedule.e2e_ns)],
    );

    // Per request: vLLM's value (alone, and plus its client_queue_time) minus
    // intended truth for the same request.
    let per_request = |name: &str, list: &[f64], truth: &[i64]| {
        for (label, with_queue) in [("alone", false), ("+ queue", true)] {
            let diffs: Vec<f64> = lines
                .iter()
                .zip(truth)
                .map(|(l, &t)| at(list, l) + if with_queue { at(&queue, l) } else { 0.0 } - ms(t as f64))
                .collect();
            let mut sorted = diffs.clone();
            sorted.sort_by(f64::total_cmp);
            let quantiles = [
                ("min", sorted[0]),
                ("p50", nearest_rank(&diffs, Some(0.5))),
                ("p99", nearest_rank(&diffs, Some(0.99))),
                ("max", nearest_rank(&diffs, None)),
            ];
            let text: Vec<String> = quantiles.iter().map(|(k, v)| format!("{k} {v:+.3}")).collect();
            println!("per request, vLLM {name} {label} - intended (ms): {}", text.join(", "));
            for (k, v) in quantiles {
                println!("CELL\t{id}\t{name} {label} - intended\t{k}\tper-request\t{v:.3}");
            }
        }
    };
    per_request("TTFT", &ttfts, &schedule.ttft_ns);
    per_request("E2EL", &latencies, &schedule.e2e_ns);
}

const V1: Scenario = Scenario {
    id: "V1",
    title: "rate 5, burstiness inf (constant spacing), no stall",
    rate: "5",
    burstiness: Some("inf"),
    max_concurrency: None,
    stall: None,
    schedule: Schedule::BySeq,
};
const V2: Scenario = Scenario {
    id: "V2",
    title: "rate 5, burstiness inf (constant spacing), stall 10000:2000",
    rate: "5",
    burstiness: Some("inf"),
    max_concurrency: None,
    stall: Some(STALL),
    schedule: Schedule::BySeq,
};
const V3: Scenario = Scenario {
    id: "V3",
    title: "rate 5, burstiness 1 (Poisson), stall 10000:2000",
    rate: "5",
    burstiness: Some("1"),
    max_concurrency: None,
    stall: Some(STALL),
    schedule: Schedule::None,
};
const V4: Scenario = Scenario {
    id: "V4",
    title: "rate inf, max concurrency 4 (closed loop), stall 10000:2000",
    rate: "inf",
    burstiness: None,
    max_concurrency: Some(4),
    stall: Some(STALL),
    schedule: Schedule::None,
};

const V5: Scenario = Scenario {
    id: "V5",
    title: "rate 5, burstiness inf (constant spacing), max concurrency 4, stall 10000:2000",
    rate: "5",
    burstiness: Some("inf"),
    max_concurrency: Some(4),
    stall: Some(STALL),
    schedule: Schedule::ByClientId,
};

#[test]
#[ignore = "runs the external vLLM binary for about 30 s"]
fn v1_constant_no_stall() {
    report("V1", &run_scenario(&V1));
}

#[test]
#[ignore = "runs the external vLLM binary for about 30 s"]
fn v2_constant_stall() {
    report("V2", &run_scenario(&V2));
}

#[test]
#[ignore = "runs the external vLLM binary for about 30 s"]
fn v3_poisson_stall() {
    report("V3", &run_scenario(&V3));
}

#[test]
#[ignore = "runs the external vLLM binary for about 30 s"]
fn v4_closed_loop_stall() {
    report("V4", &run_scenario(&V4));
}

#[test]
#[ignore = "runs the external vLLM binary for about 30 s"]
fn v5_constant_concurrency_stall() {
    let run = run_scenario(&V5);
    report("V5", &run);
    report_queue("V5", &run);
}

// Checks of the helpers above, without vLLM.

#[test]
fn vllm_bin_is_required() {
    let unset = resolve_vllm_bin(None).unwrap_err();
    assert!(unset.contains(VLLM_ENV) && unset.contains("not set"), "{unset}");
    let missing = resolve_vllm_bin(Some("/nonexistent/vllm".into())).unwrap_err();
    assert!(
        missing.contains("/nonexistent/vllm") && missing.contains("not a file"),
        "{missing}"
    );
}

#[test]
fn python_json_constants_are_read() {
    let text = r#"{"request_rate": Infinity, "burstiness": -Infinity, "x": NaN, "rate": 5.0}"#;
    let json: Value = serde_json::from_str(&python_json(text)).unwrap();
    assert_eq!(json["request_rate"], "Infinity");
    assert_eq!(json["burstiness"], "-Infinity");
    assert_eq!(json["x"], "NaN");
    assert_eq!(json["rate"], 5.0);
}

/// Expected values from numpy 2.4.6's `np.percentile` (run in vLLM's venv),
/// compared exactly: the replica interpolates as numpy's `_lerp` does.
#[test]
fn numpy_percentile_matches_numpy() {
    let check = |values: &[f64], expected: [f64; 4]| {
        let got = [50.0, 90.0, 99.0, 100.0].map(|p| numpy_percentile(values, p));
        assert_eq!(got, expected, "{values:?}");
    };
    let range = |n: u32| (1..=n).map(f64::from).collect::<Vec<_>>();
    check(&range(150), [75.5, 135.1, 148.51, 150.0]);
    check(&range(10), [5.5, 9.1, 9.91, 10.0]);
    let mixed: Vec<f64> = (1..=147u32).map(|i| f64::from(i * 7919 % 1000) / 7.0).collect();
    check(
        &mixed,
        [
            70.71428571428571,
            127.88571428571429,
            139.94571428571427,
            140.57142857142858,
        ],
    );
    check(
        &[5.0, 5.0, 1.0, 3.0, 3.0, 3.0, 9.0],
        [3.0, 6.600000000000001, 8.759999999999998, 9.0],
    );
    check(&[2.5], [2.5; 4]);
    // Nearest rank picks a sample; numpy interpolates between two.
    assert_eq!(nearest_rank(&range(150), Some(0.5)), 75.0);
    assert_eq!(nearest_rank(&range(10), Some(0.9)), 9.0);
}

/// Lines without a client_id (requests sent without vLLM's request id) are
/// taken out before any truth is computed, and the rest keep their seqs.
#[test]
fn untagged_lines_are_taken_out() {
    let log = [
        r#"{"type":"manifest"}"#,
        r#"{"type":"req","seq":0,"arrival_ns":0,"sent_ns":900,"stream":true,"token_ns":[500,700]}"#,
        r#"{"type":"req","seq":1,"arrival_ns":1000,"sent_ns":1900,"client_id":0,"stream":true,"token_ns":[1500,1700]}"#,
        r#"{"type":"req","seq":2,"arrival_ns":2100,"sent_ns":3000,"client_id":1,"stream":true,"token_ns":[2600,2800]}"#,
    ]
    .join("\n");
    let mut log = stream_lines(&log);
    let untagged = log.take_untagged();
    assert_eq!(untagged.iter().map(|l| l.seq).collect::<Vec<_>>(), [0]);
    let kept: Vec<(u64, Option<u64>)> = log.completed.iter().map(|l| (l.seq, l.client_id)).collect();
    assert_eq!(kept, [(1, Some(0)), (2, Some(1))]);
    // Seqs 1 and 2 at period 1000: origin min(1000 - 1000, 2100 - 2000) = 0,
    // so the untagged line 0 does not move the schedule.
    let s = stream_schedule_truth(&log, 1000);
    assert_eq!(s.origin_ns, 0);
    assert_eq!(s.lateness_ns, [0, 100]);
    assert_eq!(s.ttft_ns, [500, 600]);
}

/// Behind --max-concurrency, request 1 can reach phantom before request 0:
/// intended truth follows client_id, not seq.
#[test]
fn schedule_by_client_id_follows_client_id() {
    let log = [
        r#"{"type":"manifest"}"#,
        r#"{"type":"req","seq":0,"arrival_ns":200000000,"sent_ns":900000000,"client_id":1,"stream":true,"token_ns":[300000000,400000000]}"#,
        r#"{"type":"req","seq":1,"arrival_ns":250000000,"sent_ns":950000000,"client_id":0,"stream":true,"token_ns":[350000000,450000000]}"#,
    ]
    .join("\n");
    let log = stream_lines(&log);
    let s = schedule_by_client_id(&log);
    // Request 0 at 250 ms and request 1 at 200 ms: origin min(250, 200 - 200) = 0.
    assert_eq!(s.origin_ns, 0);
    assert_eq!(s.lateness_ns, [0, 250_000_000]);
    assert_eq!(s.ttft_ns, [100_000_000, 350_000_000]);
    assert_eq!(s.e2e_ns, [700_000_000, 950_000_000]);
}
