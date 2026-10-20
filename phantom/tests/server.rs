//! Server tests: an in-process phantom on 127.0.0.1:0, driven with reqwest.
//!
//! Assertions are on the ground-truth log and on response bodies, never on
//! how long anything took by the wall clock. Where a test needs requests to
//! land in particular places (before, inside, after a stall), it checks that
//! they did as a precondition, and the exact values are then checked against
//! where they landed.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use phantom::{Phantom, Script, ServiceSpec, Stall};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, sleep_until};

const MS: u64 = 1_000_000;

struct Harness {
    phantom: Phantom,
    client: reqwest::Client,
    base: String,
    log: PathBuf,
    _dir: TempDir,
}

/// A parsed ground-truth log: the manifest line, then every req line in file order.
struct Log {
    manifest: Value,
    reqs: Vec<Value>,
}

async fn launch(script: Script) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("gt.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let phantom = phantom::start(listener, script, &log).await.unwrap();
    let base = format!("http://{}", phantom.local_addr());
    Harness {
        phantom,
        client: reqwest::Client::new(),
        base,
        log,
        _dir: dir,
    }
}

/// Sends one chat completion request and returns the parsed response body.
async fn chat(client: &reqwest::Client, base: &str) -> Value {
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "any", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

/// Sends one chat completion request with header `x-request-id: id`.
async fn chat_with_request_id(client: &reqwest::Client, base: &str, id: &str) {
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-request-id", id)
        .json(&json!({"model": "any", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
}

/// Sends `n` chat requests at once and returns their bodies.
async fn chat_concurrently(h: &Harness, n: usize) -> Vec<Value> {
    let mut set = JoinSet::new();
    for _ in 0..n {
        let (client, base) = (h.client.clone(), h.base.clone());
        set.spawn(async move { chat(&client, &base).await });
    }
    set.join_all().await
}

async fn healthz(h: &Harness) {
    let resp = h.client.get(format!("{}/healthz", h.base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

fn parse_log(text: &str) -> Log {
    let mut lines = text.lines().map(|line| serde_json::from_str::<Value>(line).unwrap());
    let manifest = lines.next().expect("log is empty");
    assert_eq!(
        manifest["type"], "manifest",
        "first line is not the manifest: {manifest}"
    );
    let reqs: Vec<Value> = lines.collect();
    for r in &reqs {
        assert_eq!(r["type"], "req", "unexpected line after the manifest: {r}");
    }
    Log { manifest, reqs }
}

impl Harness {
    async fn finish(self) -> Log {
        self.phantom.shutdown().await.unwrap();
        parse_log(&std::fs::read_to_string(&self.log).unwrap())
    }
}

fn field(r: &Value, key: &str) -> u64 {
    r[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} missing or not a u64 in {r}"))
}

/// Sorted seq values must be exactly 0..n.
fn assert_seqs_are_dense(reqs: &[Value]) {
    let mut seqs: Vec<u64> = reqs.iter().map(|r| field(r, "seq")).collect();
    seqs.sort_unstable();
    let expected: Vec<u64> = (0..reqs.len() as u64).collect();
    assert_eq!(seqs, expected, "seq values have gaps or duplicates");
}

/// What a handler must have done for every record, whatever the script.
fn assert_sent_after_scripted_delays(r: &Value) {
    let held = field(r, "sent_ns") - field(r, "arrival_ns");
    let scripted = field(r, "stall_wait_ns") + field(r, "scripted_service_ns");
    assert!(
        held >= scripted,
        "responded after {held} ns, before the scripted {scripted} ns: {r}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fixed_service_time_is_logged_exactly() {
    let h = launch(Script::new(1, ServiceSpec::Fixed { ms: 20 }, vec![]).unwrap()).await;
    let mut bodies = vec![chat(&h.client, &h.base).await, chat(&h.client, &h.base).await];
    bodies.extend(chat_concurrently(&h, 8).await);
    let log = h.finish().await;

    for body in &bodies {
        assert_eq!(body["object"], "chat.completion", "{body}");
        assert_eq!(body["model"], "phantom", "{body}");
        assert!(body["id"].is_string(), "{body}");
        let message = &body["choices"][0]["message"];
        assert_eq!(message["role"], "assistant", "{body}");
        assert!(message["content"].is_string(), "{body}");
        let usage = &body["usage"];
        assert_eq!(
            field(usage, "total_tokens"),
            field(usage, "prompt_tokens") + field(usage, "completion_tokens"),
            "{body}"
        );
    }
    assert_eq!(log.reqs.len(), bodies.len());
    for r in &log.reqs {
        assert_eq!(field(r, "scripted_service_ns"), 20 * MS, "{r}");
        assert_eq!(field(r, "stall_wait_ns"), 0, "{r}");
        assert_sent_after_scripted_delays(r);
    }
}

/// Runs `n` concurrent requests against a lognormal script and returns
/// scripted_service_ns indexed by seq.
async fn lognormal_sequence(seed: u64, n: usize) -> Vec<u64> {
    let spec = ServiceSpec::Lognormal {
        median_ms: 5.0,
        sigma: 0.5,
    };
    let script = Script::new(seed, spec, vec![]).unwrap();
    let h = launch(script.clone()).await;
    chat_concurrently(&h, n).await;
    let mut reqs = h.finish().await.reqs;
    assert_eq!(reqs.len(), n);
    assert_seqs_are_dense(&reqs);
    reqs.sort_by_key(|r| field(r, "seq"));
    reqs.iter()
        .map(|r| {
            assert_sent_after_scripted_delays(r);
            let logged = field(r, "scripted_service_ns");
            // The log agrees with the pure function the handler is scripted by.
            assert_eq!(logged, script.scripted_service_ns(field(r, "seq")), "{r}");
            logged
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn lognormal_service_times_are_a_function_of_seed_and_seq() {
    const N: usize = 32;
    let first = lognormal_sequence(42, N).await;
    let again = lognormal_sequence(42, N).await;
    let other = lognormal_sequence(43, N).await;

    assert_eq!(first, again, "same seed, different service times");
    assert_ne!(first, other, "different seeds, same service times");
    let distinct: HashSet<u64> = first.iter().copied().collect();
    assert!(distinct.len() > N / 2, "lognormal draws barely vary: {first:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stall_holds_requests_until_the_window_ends() {
    // Window [100 ms, 400 ms) after the first request's arrival.
    let (start, end) = (100 * MS, 400 * MS);
    let stall = Stall {
        start_ms: 100,
        len_ms: 300,
    };
    let h = launch(Script::new(1, ServiceSpec::Fixed { ms: 1 }, vec![stall]).unwrap()).await;

    // Arrives at t0 (it defines t0), so before the window.
    let sent0 = Instant::now();
    chat(&h.client, &h.base).await;
    // Aimed at the middle of the window, far from both edges.
    let mut set = JoinSet::new();
    for offset_ms in [200, 250, 300] {
        let (client, base) = (h.client.clone(), h.base.clone());
        set.spawn(async move {
            sleep_until(sent0 + Duration::from_millis(offset_ms)).await;
            chat(&client, &base).await
        });
    }
    set.join_all().await;
    // Sent after stalled responses came back, so after the window.
    chat(&h.client, &h.base).await;
    chat(&h.client, &h.base).await;
    let log = h.finish().await;

    let (mut before, mut inside, mut after) = (0, 0, 0);
    for r in &log.reqs {
        let arrival = field(r, "arrival_ns");
        let expected = if arrival < start {
            before += 1;
            0
        } else if arrival < end {
            inside += 1;
            end - arrival
        } else {
            after += 1;
            0
        };
        assert_eq!(field(r, "stall_wait_ns"), expected, "{r}");
        assert_eq!(field(r, "scripted_service_ns"), MS, "{r}");
        assert_sent_after_scripted_delays(r);
    }
    assert!(
        before >= 1 && inside >= 1 && after >= 1,
        "precondition: requests should land before, inside and after the window; got {before}/{inside}/{after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn log_has_a_manifest_then_one_line_per_request() {
    const N: usize = 20;
    let stalls = vec![
        Stall {
            start_ms: 60_000,
            len_ms: 10,
        },
        Stall {
            start_ms: 90_000,
            len_ms: 25,
        },
    ];
    let h = launch(Script::new(9, ServiceSpec::Fixed { ms: 2 }, stalls).unwrap()).await;
    let addr = h.phantom.local_addr().to_string();

    // Health checks before the first request must not start the clock or log.
    healthz(&h).await;
    healthz(&h).await;
    let (bodies, ()) = tokio::join!(chat_concurrently(&h, N), async {
        for _ in 0..5 {
            healthz(&h).await;
        }
    });
    healthz(&h).await;
    assert_eq!(bodies.len(), N);
    // client_id is logged only for an x-request-id that parses as a u64.
    let valid = ["0", "7", "18446744073709551615"];
    for id in valid.iter().chain(&["-1", "abc", "18446744073709551616", ""]) {
        chat_with_request_id(&h.client, &h.base, id).await;
    }
    let log = h.finish().await;

    let m = &log.manifest;
    assert_eq!(m["phantom_version"], env!("CARGO_PKG_VERSION"), "{m}");
    assert_eq!(m["seed"], 9, "{m}");
    assert_eq!(m["service"], json!({"kind": "fixed", "ms": 2}), "{m}");
    assert_eq!(
        m["stalls"],
        json!([{"start_ms": 60_000, "len_ms": 10}, {"start_ms": 90_000, "len_ms": 25}]),
        "{m}"
    );
    assert_eq!(m["listen"], addr, "{m}");
    assert!(field(m, "start_unix_ns") > 0, "{m}");

    assert_eq!(log.reqs.len(), N + 7, "expected exactly one req line per chat request");
    let keys: BTreeSet<&str> = [
        "type",
        "seq",
        "arrival_ns",
        "scripted_service_ns",
        "stall_wait_ns",
        "sent_ns",
    ]
    .into();
    let mut client_ids = Vec::new();
    for r in &log.reqs {
        let mut got: BTreeSet<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
        if got.remove("client_id") {
            client_ids.push(field(r, "client_id"));
        }
        assert_eq!(got, keys, "{r}");
    }
    client_ids.sort_unstable();
    let expected: Vec<u64> = valid.iter().map(|id| id.parse().unwrap()).collect();
    assert_eq!(
        client_ids, expected,
        "client_id must be logged exactly for valid u64 headers"
    );
    assert_seqs_are_dense(&log.reqs);
}

/// The log is written while the server runs, not only at shutdown.
#[tokio::test(flavor = "multi_thread")]
async fn log_is_flushed_while_running() {
    let h = launch(Script::new(1, ServiceSpec::Fixed { ms: 1 }, vec![]).unwrap()).await;
    chat(&h.client, &h.base).await;
    // Hang guard only: the writer flushes at least every 100 ms.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(&h.log).unwrap_or_default();
        // Only whole lines: the writer may be mid-write.
        if text.ends_with('\n') && text.lines().count() == 2 {
            let log = parse_log(&text);
            assert_eq!(field(&log.reqs[0], "seq"), 0);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "req line not flushed while running; log: {text:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
    h.finish().await;
}
