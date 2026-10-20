//! Streaming tests: phantom streams Server-Sent Events like an LLM server,
//! with scripted time to first token (TTFT) and inter-token latency (ITL), and
//! logs per-token ground truth.
//!
//! Assertions are on parsed events and logged values, never on how long
//! anything took by the wall clock. Where a test needs a token to land inside
//! a stall, it checks that one did as a precondition, and the exact values are
//! then checked against where the tokens landed.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use phantom::{
    Phantom, Script, ServiceSpec, Stall, StreamSpec, parse_itl_lognormal, parse_itl_ms, parse_output_tokens,
    parse_ttft_lognormal, parse_ttft_ms,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

const MS: u64 = 1_000_000;
const CHAT: &str = "/v1/chat/completions";
const COMPLETIONS: &str = "/v1/completions";
/// Exactly the fields a non-streaming line has always had.
const OLD_KEYS: [&str; 6] = [
    "type",
    "seq",
    "arrival_ns",
    "scripted_service_ns",
    "stall_wait_ns",
    "sent_ns",
];
const STREAM_KEYS: [&str; 7] = [
    "stream",
    "endpoint",
    "scripted_ttft_ns",
    "scripted_itl_ns",
    "token_ns",
    "token_stall_wait_ns",
    "output_tokens",
];

struct Harness {
    phantom: Phantom,
    client: reqwest::Client,
    base: String,
    log: PathBuf,
    _dir: TempDir,
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

/// A response: content type and the whole body.
struct Reply {
    content_type: String,
    body: String,
}

async fn post(client: &reqwest::Client, base: &str, path: &str, body: String) -> Reply {
    let resp = client
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    Reply {
        content_type,
        body: resp.text().await.unwrap(),
    }
}

async fn post_json(h: &Harness, path: &str, body: Value) -> Reply {
    post(&h.client, &h.base, path, body.to_string()).await
}

/// The `data:` payload of every event, in order. Each event must be exactly
/// one `data: ...` line followed by a blank line.
fn sse_payloads(reply: &Reply) -> Vec<String> {
    assert!(
        reply.content_type.starts_with("text/event-stream"),
        "content-type {:?}",
        reply.content_type
    );
    let body = reply.body.strip_suffix("\n\n").expect("stream ends with a blank line");
    body.split("\n\n")
        .map(|event| {
            event
                .strip_prefix("data: ")
                .filter(|data| !data.contains('\n'))
                .unwrap_or_else(|| panic!("not a single data line: {event:?}"))
                .to_owned()
        })
        .collect()
}

/// Parses every payload but the final `[DONE]`.
fn sse_chunks(reply: &Reply) -> Vec<Value> {
    let mut payloads = sse_payloads(reply);
    assert_eq!(payloads.pop().as_deref(), Some("[DONE]"), "last event is not [DONE]");
    payloads.iter().map(|p| serde_json::from_str(p).unwrap()).collect()
}

impl Harness {
    /// Shuts down and returns (manifest, req lines in file order).
    async fn finish(self) -> (Value, Vec<Value>) {
        self.phantom.shutdown().await.unwrap();
        let text = std::fs::read_to_string(&self.log).unwrap();
        let mut lines = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap());
        let manifest = lines.next().unwrap();
        assert_eq!(manifest["type"], "manifest");
        (manifest, lines.collect())
    }
}

fn field(r: &Value, key: &str) -> u64 {
    r[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} missing or not a u64 in {r}"))
}

fn array(r: &Value, key: &str) -> Vec<u64> {
    r[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} missing or not an array in {r}"))
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect()
}

fn keys(r: &Value) -> BTreeSet<&str> {
    r.as_object().unwrap().keys().map(String::as_str).collect()
}

fn script(seed: u64, stalls: Vec<Stall>, stream: StreamSpec) -> Script {
    Script::new(seed, ServiceSpec::Fixed { ms: 1 }, stalls)
        .unwrap()
        .with_stream(stream)
        .unwrap()
}

fn fixed(ms: u64) -> ServiceSpec {
    ServiceSpec::Fixed { ms }
}

fn chat_body(extra: Value) -> Value {
    let mut body = json!({"model": "any", "messages": [{"role": "user", "content": "hi"}], "stream": true});
    body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    body
}

/// What every streaming line must satisfy, whatever the script: the shape of
/// the streaming fields, and that no token was handed off before its scripted
/// schedule (TTFT from arrival, then each ITL from the previous hand-off, then
/// any stall wait).
fn check_stream_line(r: &Value, endpoint: &str, tokens: usize) {
    let mut expected: BTreeSet<&str> = OLD_KEYS.into();
    expected.extend(STREAM_KEYS);
    assert_eq!(keys(r), expected, "{r}");
    assert_eq!(r["stream"], true, "{r}");
    assert_eq!(r["endpoint"], endpoint, "{r}");
    assert_eq!(field(r, "output_tokens"), tokens as u64, "{r}");
    let (ttft, itl) = (field(r, "scripted_ttft_ns"), array(r, "scripted_itl_ns"));
    let (token_ns, waits) = (array(r, "token_ns"), array(r, "token_stall_wait_ns"));
    assert_eq!(token_ns.len(), tokens, "{r}");
    assert_eq!(waits.len(), tokens, "{r}");
    assert_eq!(itl.len(), tokens - 1, "one ITL per gap: {r}");
    assert!(
        token_ns.windows(2).all(|w| w[0] < w[1]),
        "token_ns not strictly increasing: {r}"
    );

    let arrival = field(r, "arrival_ns");
    let mut scheduled = arrival + ttft;
    for k in 0..tokens {
        if k > 0 {
            scheduled = token_ns[k - 1] + itl[k - 1];
            assert!(
                token_ns[k] - token_ns[k - 1] >= itl[k - 1],
                "gap {k} below its ITL: {r}"
            );
        }
        assert!(token_ns[k] >= scheduled + waits[k], "token {k} handed off early: {r}");
    }
    assert!(token_ns[0] - arrival >= ttft, "{r}");
    // The old fields keep their meaning for streams: the scripted total and
    // the total time stalls added.
    assert_eq!(field(r, "scripted_service_ns"), ttft + itl.iter().sum::<u64>(), "{r}");
    assert_eq!(field(r, "stall_wait_ns"), waits.iter().sum::<u64>(), "{r}");
    let sent = field(r, "sent_ns");
    assert!(sent >= token_ns[tokens - 1], "{r}");
    assert!(
        sent - arrival >= field(r, "stall_wait_ns") + field(r, "scripted_service_ns"),
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_stream_has_scripted_ttft_and_itl() {
    let stream = StreamSpec {
        ttft: Some(fixed(50)),
        itl: fixed(10),
        output_tokens: 8,
    };
    let h = launch(script(1, vec![], stream)).await;
    let reply = post_json(&h, CHAT, chat_body(json!({}))).await;
    let (manifest, reqs) = h.finish().await;

    let chunks = sse_chunks(&reply);
    assert_eq!(chunks.len(), 1 + 8 + 1, "role, 8 tokens, finish: {chunks:?}");
    for c in &chunks {
        assert_eq!(c["object"], "chat.completion.chunk", "{c}");
        assert_eq!(c["model"], "phantom", "{c}");
        assert_eq!(c["id"], chunks[0]["id"], "one id per stream: {c}");
    }
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    for c in &chunks[1..9] {
        assert_eq!(c["choices"][0]["delta"]["content"], " tok", "{c}");
        assert!(c["choices"][0]["finish_reason"].is_null(), "{c}");
    }
    let last = &chunks[9];
    assert_eq!(last["choices"][0]["finish_reason"], "length", "{last}");
    assert_eq!(field(&last["usage"], "completion_tokens"), 8, "{last}");
    assert_eq!(
        field(&last["usage"], "total_tokens"),
        field(&last["usage"], "prompt_tokens") + 8,
        "{last}"
    );

    assert_eq!(manifest["ttft"], json!({"kind": "fixed", "ms": 50}), "{manifest}");
    assert_eq!(manifest["itl"], json!({"kind": "fixed", "ms": 10}), "{manifest}");
    assert_eq!(manifest["output_tokens"], 8, "{manifest}");

    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    check_stream_line(r, CHAT, 8);
    assert_eq!(field(r, "scripted_ttft_ns"), 50 * MS, "{r}");
    assert_eq!(array(r, "scripted_itl_ns"), vec![10 * MS; 7], "{r}");
    assert_eq!(array(r, "token_stall_wait_ns"), vec![0; 8], "{r}");
}

/// Streams one chat request and returns the number of content chunks.
async fn content_chunks(h: &Harness, extra: Value) -> usize {
    let chunks = sse_chunks(&post_json(h, CHAT, chat_body(extra)).await);
    let content = chunks
        .iter()
        .filter(|c| c["choices"][0]["delta"]["content"] == " tok")
        .count();
    assert_eq!(
        field(&chunks.last().unwrap()["usage"], "completion_tokens"),
        content as u64
    );
    content
}

#[tokio::test(flavor = "multi_thread")]
async fn max_tokens_truncates_and_output_tokens_is_the_default() {
    let stream = StreamSpec {
        ttft: Some(fixed(2)),
        itl: fixed(2),
        output_tokens: 5,
    };
    let h = launch(script(1, vec![], stream)).await;
    assert_eq!(content_chunks(&h, json!({"max_tokens": 3})).await, 3);
    assert_eq!(content_chunks(&h, json!({})).await, 5);
    assert_eq!(
        content_chunks(&h, json!({"max_tokens": 100})).await,
        5,
        "min(max_tokens, N)"
    );
    // A stream always carries at least one token.
    assert_eq!(content_chunks(&h, json!({"max_tokens": 0})).await, 1);
    // Anything but a whole number is not a max_tokens.
    assert_eq!(content_chunks(&h, json!({"max_tokens": "3"})).await, 5);
    assert_eq!(content_chunks(&h, json!({"max_tokens": 2.5})).await, 5);
    let (_, mut reqs) = h.finish().await;
    reqs.sort_by_key(|r| field(r, "seq"));
    let logged: Vec<u64> = reqs.iter().map(|r| field(r, "output_tokens")).collect();
    assert_eq!(logged, [3, 5, 5, 1, 5, 5]);
    for r in &reqs {
        check_stream_line(r, CHAT, field(r, "output_tokens") as usize);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn defaults_are_service_spec_ttft_10ms_itl_and_32_tokens() {
    let script = Script::new(1, fixed(3), vec![]).unwrap();
    let h = launch(script.clone()).await;
    assert_eq!(content_chunks(&h, json!({})).await, 32);
    let (manifest, reqs) = h.finish().await;
    assert_eq!(manifest["ttft"], json!({"kind": "fixed", "ms": 3}), "{manifest}");
    assert_eq!(manifest["itl"], json!({"kind": "fixed", "ms": 10}), "{manifest}");
    assert_eq!(manifest["output_tokens"], 32, "{manifest}");
    check_stream_line(&reqs[0], CHAT, 32);
    assert_eq!(field(&reqs[0], "scripted_ttft_ns"), 3 * MS);
    assert_eq!(array(&reqs[0], "scripted_itl_ns"), vec![10 * MS; 31]);
}

/// Streams `n` requests at once and returns (scripted_ttft_ns, scripted_itl_ns)
/// indexed by seq, checked against the script's pure functions.
async fn scripted_by_seq(seed: u64, n: usize) -> Vec<(u64, Vec<u64>)> {
    let stream = StreamSpec {
        ttft: Some(ServiceSpec::Lognormal {
            median_ms: 5.0,
            sigma: 0.5,
        }),
        itl: ServiceSpec::Lognormal {
            median_ms: 2.0,
            sigma: 0.5,
        },
        output_tokens: 4,
    };
    let script = script(seed, vec![], stream);
    let h = launch(script.clone()).await;
    let mut set = JoinSet::new();
    for _ in 0..n {
        let (client, base) = (h.client.clone(), h.base.clone());
        set.spawn(async move { post(&client, &base, CHAT, chat_body(json!({})).to_string()).await });
    }
    set.join_all().await;
    let (_, mut reqs) = h.finish().await;
    reqs.sort_by_key(|r| field(r, "seq"));
    let seqs: Vec<u64> = reqs.iter().map(|r| field(r, "seq")).collect();
    assert_eq!(seqs, (0..n as u64).collect::<Vec<_>>());
    reqs.iter()
        .map(|r| {
            check_stream_line(r, CHAT, 4);
            let seq = field(r, "seq");
            let (ttft, itl) = (field(r, "scripted_ttft_ns"), array(r, "scripted_itl_ns"));
            assert_eq!(ttft, script.scripted_ttft_ns(seq), "{r}");
            let expected: Vec<u64> = (1..4).map(|k| script.scripted_itl_ns(seq, k)).collect();
            assert_eq!(itl, expected, "{r}");
            (ttft, itl)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn lognormal_ttft_and_itl_are_a_function_of_seed_seq_and_token() {
    const N: usize = 16;
    let first = scripted_by_seq(42, N).await;
    let again = scripted_by_seq(42, N).await;
    let other = scripted_by_seq(43, N).await;
    assert_eq!(first, again, "same seed, different scripted arrays");
    assert_ne!(first, other, "different seeds, same scripted arrays");
    let ttfts: BTreeSet<u64> = first.iter().map(|(t, _)| *t).collect();
    let itls: BTreeSet<u64> = first.iter().flat_map(|(_, i)| i.iter().copied()).collect();
    assert!(ttfts.len() > N / 2, "TTFT draws barely vary: {first:?}");
    assert!(itls.len() > N, "ITL draws barely vary: {first:?}");
}

#[test]
fn ttft_itl_and_service_draws_are_distinct_streams() {
    // TTFT defaults to the service spec, but draws from its own stream.
    let lognormal = ServiceSpec::Lognormal {
        median_ms: 20.0,
        sigma: 0.5,
    };
    let script = Script::new(7, lognormal, vec![])
        .unwrap()
        .with_stream(StreamSpec {
            ttft: None,
            itl: lognormal,
            output_tokens: 8,
        })
        .unwrap();
    let same_as_service = (0..200)
        .filter(|&seq| script.scripted_ttft_ns(seq) == script.scripted_service_ns(seq))
        .count();
    assert_eq!(same_as_service, 0, "TTFT reuses the service-time draw");
    let ttft_as_itl = (0..200)
        .filter(|&seq| script.scripted_ttft_ns(seq) == script.scripted_itl_ns(seq, 1))
        .count();
    assert_eq!(ttft_as_itl, 0, "TTFT and ITL share draws");
    let one_seq: BTreeSet<u64> = (1..64).map(|k| script.scripted_itl_ns(3, k)).collect();
    assert_eq!(one_seq.len(), 63, "ITL does not vary by token index");
    // Pure: asking twice gives the same answer.
    assert_eq!(script.scripted_itl_ns(3, 5), script.scripted_itl_ns(3, 5));
}

#[tokio::test(flavor = "multi_thread")]
async fn stall_delays_only_the_token_scheduled_inside_it() {
    // t0 is this request's arrival. Tokens are scheduled about 50, 70, 90,
    // 110, ... ms after it (plus timer overshoot), so one lands inside the
    // window [100 ms, 160 ms) and is emitted at its end.
    let (start, end) = (100 * MS, 160 * MS);
    let stall = Stall {
        start_ms: 100,
        len_ms: 60,
    };
    let stream = StreamSpec {
        ttft: Some(fixed(50)),
        itl: fixed(20),
        output_tokens: 10,
    };
    let h = launch(script(1, vec![stall], stream)).await;
    content_chunks(&h, json!({})).await;
    let (_, reqs) = h.finish().await;
    let r = &reqs[0];
    check_stream_line(r, CHAT, 10);

    let (token_ns, waits, itl) = (
        array(r, "token_ns"),
        array(r, "token_stall_wait_ns"),
        array(r, "scripted_itl_ns"),
    );
    let mut stalled = Vec::new();
    for k in 0..10 {
        let scheduled = if k == 0 {
            field(r, "arrival_ns") + field(r, "scripted_ttft_ns")
        } else {
            token_ns[k - 1] + itl[k - 1]
        };
        let expected = if (start..end).contains(&scheduled) {
            stalled.push(k);
            end - scheduled
        } else {
            0
        };
        assert_eq!(waits[k], expected, "token {k}: {r}");
    }
    assert_eq!(
        stalled.len(),
        1,
        "precondition: exactly one token scheduled inside the window: {r}"
    );
    let k = stalled[0];
    assert!(
        k > 0 && k < 9,
        "precondition: the stall covers the middle of the stream: {r}"
    );
    assert!(token_ns[k] >= end, "{r}");
    // After the stalled token, every gap is scheduled at its scripted ITL and
    // nothing else waits.
    assert!(waits[k + 1..].iter().all(|&w| w == 0), "{r}");
    assert!(itl[k..].iter().all(|&g| g == 20 * MS), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn completions_stream_has_the_same_timing_and_log_fields() {
    let stream = StreamSpec {
        ttft: Some(fixed(50)),
        itl: fixed(10),
        output_tokens: 4,
    };
    let script = script(1, vec![], stream);
    let h = launch(script.clone()).await;
    let reply = post_json(&h, COMPLETIONS, json!({"model": "any", "prompt": "hi", "stream": true})).await;
    let (_, reqs) = h.finish().await;

    let chunks = sse_chunks(&reply);
    assert_eq!(chunks.len(), 4 + 1, "4 tokens, finish: {chunks:?}");
    for c in &chunks {
        assert_eq!(c["object"], "text_completion", "{c}");
        assert_eq!(c["model"], "phantom", "{c}");
    }
    for c in &chunks[..4] {
        assert_eq!(c["choices"][0]["text"], " tok", "{c}");
        assert!(c["choices"][0]["finish_reason"].is_null(), "{c}");
    }
    assert_eq!(chunks[4]["choices"][0]["finish_reason"], "length");
    assert_eq!(field(&chunks[4]["usage"], "completion_tokens"), 4);

    let r = &reqs[0];
    check_stream_line(r, COMPLETIONS, 4);
    assert_eq!(field(r, "scripted_ttft_ns"), 50 * MS, "{r}");
    assert_eq!(array(r, "scripted_itl_ns"), vec![10 * MS; 3], "{r}");
    assert_eq!(field(r, "scripted_ttft_ns"), script.scripted_ttft_ns(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn non_streaming_requests_log_exactly_the_old_fields() {
    let stream = StreamSpec {
        ttft: Some(fixed(50)),
        itl: fixed(10),
        output_tokens: 4,
    };
    let h = launch(script(1, vec![], stream)).await;
    let not_streams = [
        (CHAT, json!({"model": "any", "messages": []}).to_string()),
        (CHAT, json!({"stream": false, "max_tokens": 2}).to_string()),
        (CHAT, json!({"stream": "true"}).to_string()),
        (CHAT, "not json".to_owned()),
        (COMPLETIONS, json!({"model": "any", "prompt": "hi"}).to_string()),
        (COMPLETIONS, json!({"stream": false}).to_string()),
    ];
    for (path, body) in &not_streams {
        let reply = post(&h.client, &h.base, path, body.clone()).await;
        assert!(reply.content_type.starts_with("application/json"), "{path} {body}");
        let b: Value = serde_json::from_str(&reply.body).unwrap();
        if *path == CHAT {
            assert_eq!(b["object"], "chat.completion", "{b}");
            assert_eq!(b["choices"][0]["message"]["role"], "assistant", "{b}");
        } else {
            assert_eq!(b["object"], "text_completion", "{b}");
            assert!(b["choices"][0]["text"].is_string(), "{b}");
            assert_eq!(b["choices"][0]["finish_reason"], "stop", "{b}");
            let usage = &b["usage"];
            assert_eq!(
                field(usage, "total_tokens"),
                field(usage, "prompt_tokens") + field(usage, "completion_tokens"),
                "{b}"
            );
        }
    }
    let (_, reqs) = h.finish().await;
    assert_eq!(reqs.len(), not_streams.len());
    let old: BTreeSet<&str> = OLD_KEYS.into();
    for r in &reqs {
        assert_eq!(keys(r), old, "{r}");
        assert_eq!(field(r, "scripted_service_ns"), MS, "{r}");
    }
}

#[test]
fn stream_specs_are_validated() {
    assert_eq!(parse_ttft_ms("50"), Ok(fixed(50)));
    assert_eq!(parse_itl_ms("10"), Ok(fixed(10)));
    assert_eq!(
        parse_itl_lognormal("2:0.5"),
        Ok(ServiceSpec::Lognormal {
            median_ms: 2.0,
            sigma: 0.5
        })
    );
    assert_eq!(parse_output_tokens("32"), Ok(32));
    for bad in ["0", "-1", "abc", "", "1.5"] {
        let ttft = parse_ttft_ms(bad).expect_err(bad).to_string();
        assert!(ttft.contains("TTFT") && ttft.contains(bad), "{ttft}");
        let itl = parse_itl_ms(bad).expect_err(bad).to_string();
        assert!(itl.contains("ITL") && itl.contains(bad), "{itl}");
        let tokens = parse_output_tokens(bad).expect_err(bad).to_string();
        assert!(tokens.contains("output tokens") && tokens.contains(bad), "{tokens}");
    }
    for bad in ["0:0.5", "5:0", "5", "a:b", "5:0.5:1", "nan:1"] {
        parse_ttft_lognormal(bad).expect_err(bad);
        parse_itl_lognormal(bad).expect_err(bad);
    }
    let base = || Script::new(1, fixed(1), vec![]).unwrap();
    let spec = |ttft, itl, output_tokens| StreamSpec {
        ttft,
        itl,
        output_tokens,
    };
    base().with_stream(spec(None, fixed(10), 0)).expect_err("zero tokens");
    base().with_stream(spec(None, fixed(0), 8)).expect_err("zero ITL");
    base()
        .with_stream(spec(Some(fixed(0)), fixed(10), 8))
        .expect_err("zero TTFT");
    base().with_stream(spec(None, fixed(10), 8)).expect("valid");
}

#[test]
fn cli_refuses_invalid_stream_flags() {
    let refused = |args: &[&str], mentions: &str| {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("gt.jsonl");
        let out = Command::new(env!("CARGO_BIN_EXE_phantom"))
            .args(["--listen", "127.0.0.1:0", "--service-ms", "1", "--log"])
            .arg(&log)
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} was accepted");
        assert!(stderr.contains(mentions), "{args:?}: {stderr}");
        assert!(!log.exists(), "{args:?}: log written for a refused config");
    };
    refused(&["--itl-ms", "0"], "--itl-ms");
    refused(&["--ttft-ms", "0"], "--ttft-ms");
    refused(&["--output-tokens", "0"], "--output-tokens");
    refused(&["--itl-lognormal", "2"], "2");
    refused(&["--ttft-ms", "5", "--ttft-lognormal", "5:0.5"], "--ttft-lognormal");
    refused(&["--itl-ms", "5", "--itl-lognormal", "5:0.5"], "--itl-lognormal");
}
