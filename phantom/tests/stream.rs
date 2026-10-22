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
    DEFAULT_TOKEN_TEXT, Phantom, RoleChunk, Script, ServiceSpec, Stall, StreamSpec, parse_itl_lognormal, parse_itl_ms,
    parse_output_tokens, parse_role_chunk, parse_token_text, parse_ttft_lognormal, parse_ttft_ms,
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
const STREAM_KEYS: [&str; 8] = [
    "stream",
    "endpoint",
    "scripted_ttft_ns",
    "scripted_itl_ns",
    "token_ns",
    "scheduled_token_ns",
    "token_stall_wait_ns",
    "output_tokens",
];
/// Chat streams only.
const ROLE_CHUNK_KEY: &str = "role_chunk_ns";

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

/// A streaming chat body that asks for the usage chunk, as vLLM's and
/// GuideLLM's clients do.
fn chat_body(extra: Value) -> Value {
    let mut body = json!({
        "model": "any",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    body
}

/// What every streaming line must satisfy, whatever the script: the shape of
/// the streaming fields, the exact scripted draws and absolute schedule
/// (token 0 at arrival + TTFT, token k at the previous scheduled time +
/// ITL_k, shifted to a stall window's end, with every later scheduled time
/// shifted with it), and that no token was handed off before its schedule.
fn check_stream_line(r: &Value, script: &Script, endpoint: &str, tokens: usize) {
    let mut expected: BTreeSet<&str> = OLD_KEYS.into();
    expected.extend(STREAM_KEYS);
    if endpoint == CHAT {
        expected.insert(ROLE_CHUNK_KEY);
    }
    assert_eq!(keys(r), expected, "{r}");
    assert_eq!(r["stream"], true, "{r}");
    assert_eq!(r["endpoint"], endpoint, "{r}");
    assert_eq!(field(r, "output_tokens"), tokens as u64, "{r}");
    let seq = field(r, "seq");
    let (ttft, itl) = (field(r, "scripted_ttft_ns"), array(r, "scripted_itl_ns"));
    let (token_ns, scheduled, waits) = (
        array(r, "token_ns"),
        array(r, "scheduled_token_ns"),
        array(r, "token_stall_wait_ns"),
    );
    assert_eq!(ttft, script.scripted_ttft_ns(seq), "{r}");
    let expected_itl: Vec<u64> = (1..tokens as u64).map(|k| script.scripted_itl_ns(seq, k)).collect();
    assert_eq!(itl, expected_itl, "one ITL per gap, from the script: {r}");
    assert_eq!(token_ns.len(), tokens, "{r}");
    assert!(
        token_ns.windows(2).all(|w| w[0] < w[1]),
        "token_ns not strictly increasing: {r}"
    );

    let arrival = field(r, "arrival_ns");
    let (mut expected_scheduled, mut expected_waits) = (Vec::new(), Vec::new());
    let mut at = arrival + ttft;
    for k in 0..tokens {
        if k > 0 {
            at += itl[k - 1];
        }
        let wait = script.stall_wait_ns(at);
        at += wait;
        expected_scheduled.push(at);
        expected_waits.push(wait);
    }
    assert_eq!(scheduled, expected_scheduled, "scheduled_token_ns: {r}");
    assert_eq!(waits, expected_waits, "token_stall_wait_ns: {r}");
    for k in 0..tokens {
        assert!(token_ns[k] >= scheduled[k], "token {k} handed off early: {r}");
    }
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
    if endpoint == CHAT {
        let role = field(r, ROLE_CHUNK_KEY);
        assert!(role >= arrival && role <= token_ns[0], "role chunk after token 0: {r}");
        if script.role_chunk() == RoleChunk::WithFirstToken {
            assert!(role >= scheduled[0], "role chunk before token 0's schedule: {r}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_stream_has_scripted_ttft_and_itl() {
    let stream = StreamSpec {
        ttft: Some(fixed(50)),
        itl: fixed(10),
        output_tokens: 8,
    };
    let script = script(1, vec![], stream);
    let h = launch(script.clone()).await;
    let reply = post_json(&h, CHAT, chat_body(json!({}))).await;
    let (manifest, reqs) = h.finish().await;

    let chunks = sse_chunks(&reply);
    assert_eq!(chunks.len(), 1 + 8 + 1, "role, 8 tokens, usage: {chunks:?}");
    for c in &chunks {
        assert_eq!(c["object"], "chat.completion.chunk", "{c}");
        assert_eq!(c["model"], "phantom", "{c}");
        assert_eq!(c["id"], chunks[0]["id"], "one id per stream: {c}");
    }
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    for c in &chunks[1..9] {
        assert_eq!(c["choices"][0]["delta"]["content"], " tok", "{c}");
    }
    for c in &chunks[1..8] {
        assert!(c["choices"][0]["finish_reason"].is_null(), "{c}");
    }
    assert_eq!(chunks[8]["choices"][0]["finish_reason"], "length", "{:?}", chunks[8]);
    let last = &chunks[9];
    assert_eq!(last["choices"], json!([]), "{last}");
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
    check_stream_line(r, &script, CHAT, 8);
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
    let script = script(1, vec![], stream);
    let h = launch(script.clone()).await;
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
        check_stream_line(r, &script, CHAT, field(r, "output_tokens") as usize);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn max_completion_tokens_truncates_and_wins_over_max_tokens() {
    let stream = StreamSpec {
        ttft: Some(fixed(2)),
        itl: fixed(2),
        output_tokens: 5,
    };
    let script = script(1, vec![], stream);
    let h = launch(script.clone()).await;
    assert_eq!(content_chunks(&h, json!({"max_completion_tokens": 3})).await, 3);
    assert_eq!(
        content_chunks(&h, json!({"max_completion_tokens": 100})).await,
        5,
        "min(max_completion_tokens, N)"
    );
    assert_eq!(content_chunks(&h, json!({"max_completion_tokens": 0})).await, 1);
    // Both present: max_completion_tokens wins, whether smaller or larger.
    assert_eq!(
        content_chunks(&h, json!({"max_tokens": 4, "max_completion_tokens": 2})).await,
        2
    );
    assert_eq!(
        content_chunks(&h, json!({"max_tokens": 2, "max_completion_tokens": 4})).await,
        4
    );
    // Anything but a whole number is not a max_completion_tokens, so max_tokens applies.
    assert_eq!(
        content_chunks(&h, json!({"max_tokens": 2, "max_completion_tokens": "4"})).await,
        2
    );
    let completions = post_json(
        &h,
        COMPLETIONS,
        json!({"prompt": "hi", "stream": true, "max_tokens": 4, "max_completion_tokens": 3}),
    )
    .await;
    assert_eq!(sse_chunks(&completions).len(), 3, "3 tokens, no usage chunk");
    let (_, mut reqs) = h.finish().await;
    reqs.sort_by_key(|r| field(r, "seq"));
    let logged: Vec<u64> = reqs.iter().map(|r| field(r, "output_tokens")).collect();
    assert_eq!(logged, [3, 5, 1, 2, 4, 2, 3]);
    for r in &reqs[..6] {
        check_stream_line(r, &script, CHAT, field(r, "output_tokens") as usize);
    }
    check_stream_line(&reqs[6], &script, COMPLETIONS, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn token_text_is_carried_by_every_content_chunk_and_recorded() {
    let stream = StreamSpec {
        ttft: Some(fixed(2)),
        itl: fixed(2),
        output_tokens: 3,
    };
    let script = script(1, vec![], stream).with_token_text(" the").unwrap();
    assert_eq!(script.token_text(), " the");
    let h = launch(script.clone()).await;
    let chat = sse_chunks(&post_json(&h, CHAT, chat_body(json!({}))).await);
    let completions = sse_chunks(&post_json(&h, COMPLETIONS, json!({"prompt": "hi", "stream": true})).await);
    let (manifest, mut reqs) = h.finish().await;

    assert_eq!(chat.len(), 1 + 3 + 1, "role, 3 tokens, usage: {chat:?}");
    assert_eq!(
        chat[0]["choices"][0]["delta"]["content"], "",
        "role chunk: {:?}",
        chat[0]
    );
    for c in &chat[1..4] {
        assert_eq!(c["choices"][0]["delta"]["content"], " the", "{c}");
    }
    assert_eq!(completions.len(), 3, "3 tokens, no usage chunk: {completions:?}");
    for c in &completions[..3] {
        assert_eq!(c["choices"][0]["text"], " the", "{c}");
    }
    assert_eq!(field(&chat[4]["usage"], "completion_tokens"), 3);
    assert_eq!(manifest["token_text"], " the", "{manifest}");
    reqs.sort_by_key(|r| field(r, "seq"));
    check_stream_line(&reqs[0], &script, CHAT, 3);
    check_stream_line(&reqs[1], &script, COMPLETIONS, 3);
}

#[test]
fn token_text_is_validated() {
    assert_eq!(DEFAULT_TOKEN_TEXT, " tok");
    assert_eq!(
        Script::new(1, fixed(1), vec![]).unwrap().token_text(),
        DEFAULT_TOKEN_TEXT
    );
    assert_eq!(parse_token_text(" the"), Ok(" the".to_owned()));
    let err = parse_token_text("").expect_err("empty").to_string();
    assert!(err.contains("token text"), "{err}");
    Script::new(1, fixed(1), vec![])
        .unwrap()
        .with_token_text("")
        .expect_err("empty token text");
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
    assert_eq!(manifest["token_text"], " tok", "{manifest}");
    check_stream_line(&reqs[0], &script, CHAT, 32);
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
            // Checks the draws against the script's pure functions.
            check_stream_line(r, &script, CHAT, 4);
            (field(r, "scripted_ttft_ns"), array(r, "scripted_itl_ns"))
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
async fn stall_shifts_the_rest_of_the_schedule_and_keeps_its_gaps() {
    // t0 is this request's arrival, a few µs before arrival_ns. Tokens are
    // scheduled at arrival + 50, 70, 90, 110, ... ms, so token 3 lands inside
    // the window [100 ms, 160 ms): it moves to 160 ms, and every later token
    // moves with it, keeping its scripted 20 ms gap.
    let end = 160 * MS;
    let stall = Stall {
        start_ms: 100,
        len_ms: 60,
    };
    let stream = StreamSpec {
        ttft: Some(fixed(50)),
        itl: fixed(20),
        output_tokens: 10,
    };
    let script = script(1, vec![stall], stream);
    let h = launch(script.clone()).await;
    content_chunks(&h, json!({})).await;
    let (_, reqs) = h.finish().await;
    let r = &reqs[0];
    check_stream_line(r, &script, CHAT, 10);

    let arrival = field(r, "arrival_ns");
    assert!(arrival < 10 * MS, "precondition: arrival close to t0: {r}");
    let (scheduled, waits) = (array(r, "scheduled_token_ns"), array(r, "token_stall_wait_ns"));
    let mut expected_waits = vec![0; 10];
    expected_waits[3] = end - (arrival + 110 * MS);
    assert_eq!(waits, expected_waits, "{r}");
    let expected: Vec<u64> = (0..10)
        .map(|k| {
            if k < 3 {
                arrival + (50 + 20 * k) * MS
            } else {
                end + (k - 3) * 20 * MS
            }
        })
        .collect();
    assert_eq!(scheduled, expected, "{r}");
}

/// Not a bound: prints how far behind the absolute schedule a 64-token stream
/// at a 10 ms ITL was handed off, the overshoot the schedule no longer lets
/// accumulate.
#[tokio::test(flavor = "multi_thread")]
async fn smoke_overshoot_of_a_64_token_stream_at_10ms_itl() {
    let stream = StreamSpec {
        ttft: Some(fixed(10)),
        itl: fixed(10),
        output_tokens: 64,
    };
    let script = script(1, vec![], stream);
    let h = launch(script.clone()).await;
    content_chunks(&h, json!({})).await;
    let (_, reqs) = h.finish().await;
    let r = &reqs[0];
    check_stream_line(r, &script, CHAT, 64);
    let (token_ns, scheduled) = (array(r, "token_ns"), array(r, "scheduled_token_ns"));
    let overshoot: Vec<u64> = token_ns.iter().zip(&scheduled).map(|(t, s)| t - s).collect();
    println!(
        "smoke: 64 tokens at ITL 10 ms: max(token_ns - scheduled_token_ns) = {:.3} ms, last token {:.3} ms",
        *overshoot.iter().max().unwrap() as f64 / MS as f64,
        overshoot[63] as f64 / MS as f64
    );
}

/// Streams one chat and one completions request with the role chunk sent
/// `role_chunk`, and returns (chat reply's chunks, chat line, completions line).
async fn role_chunk_run(role_chunk: RoleChunk) -> (Vec<Value>, Value, Value) {
    let stream = StreamSpec {
        ttft: Some(fixed(200)),
        itl: fixed(10),
        output_tokens: 3,
    };
    let script = script(1, vec![], stream).with_role_chunk(role_chunk);
    assert_eq!(script.role_chunk(), role_chunk);
    let h = launch(script.clone()).await;
    let chat = sse_chunks(&post_json(&h, CHAT, chat_body(json!({}))).await);
    let completions = post_json(&h, COMPLETIONS, json!({"prompt": "hi", "stream": true})).await;
    assert_eq!(sse_chunks(&completions).len(), 3, "no role chunk for completions");
    let (manifest, mut reqs) = h.finish().await;
    let expected_manifest = match role_chunk {
        RoleChunk::WithFirstToken => "with-first-token",
        RoleChunk::AtArrival => "at-arrival",
    };
    assert_eq!(manifest["role_chunk"], expected_manifest, "{manifest}");
    reqs.sort_by_key(|r| field(r, "seq"));
    check_stream_line(&reqs[0], &script, CHAT, 3);
    check_stream_line(&reqs[1], &script, COMPLETIONS, 3);
    (chat, reqs[0].clone(), reqs[1].clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn role_chunk_with_first_token_is_the_default() {
    assert_eq!(
        Script::new(1, fixed(1), vec![]).unwrap().role_chunk(),
        RoleChunk::WithFirstToken
    );
    let (chunks, r, _) = role_chunk_run(RoleChunk::WithFirstToken).await;
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant", "{:?}", chunks[0]);
    let role = field(&r, ROLE_CHUNK_KEY);
    // Handed off just before token 0, once its schedule (arrival + TTFT) has passed.
    assert!(role >= array(&r, "scheduled_token_ns")[0], "{r}");
    assert!(role <= array(&r, "token_ns")[0], "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn role_chunk_at_arrival_is_sent_before_ttft() {
    let (chunks, r, _) = role_chunk_run(RoleChunk::AtArrival).await;
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant", "{:?}", chunks[0]);
    assert_eq!(chunks.len(), 1 + 3 + 1, "role, 3 tokens, usage: {chunks:?}");
    let (arrival, role) = (field(&r, "arrival_ns"), field(&r, ROLE_CHUNK_KEY));
    // TTFT is 200 ms: a role chunk handed off on arrival is far ahead of it.
    assert!(role >= arrival, "{r}");
    assert!(
        role < array(&r, "scheduled_token_ns")[0],
        "role chunk waited for TTFT: {r}"
    );
}

#[test]
fn role_chunk_flag_values() {
    assert_eq!(parse_role_chunk("with-first-token"), Ok(RoleChunk::WithFirstToken));
    assert_eq!(parse_role_chunk("at-arrival"), Ok(RoleChunk::AtArrival));
    for bad in ["", "arrival", "At-Arrival", "first"] {
        let err = parse_role_chunk(bad).expect_err(bad).to_string();
        assert!(err.contains("role chunk") && err.contains("at-arrival"), "{err}");
    }
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
    let body = json!({"model": "any", "prompt": "hi", "stream": true, "stream_options": {"include_usage": true}});
    let reply = post_json(&h, COMPLETIONS, body).await;
    let (_, reqs) = h.finish().await;

    let chunks = sse_chunks(&reply);
    assert_eq!(chunks.len(), 4 + 1, "4 tokens, usage: {chunks:?}");
    for c in &chunks {
        assert_eq!(c["object"], "text_completion", "{c}");
        assert_eq!(c["model"], "phantom", "{c}");
    }
    for c in &chunks[..4] {
        assert_eq!(c["choices"][0]["text"], " tok", "{c}");
    }
    for c in &chunks[..3] {
        assert!(c["choices"][0]["finish_reason"].is_null(), "{c}");
    }
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "length");
    assert_eq!(chunks[4]["choices"], json!([]));
    assert_eq!(field(&chunks[4]["usage"], "completion_tokens"), 4);

    let r = &reqs[0];
    check_stream_line(r, &script, COMPLETIONS, 4);
    assert_eq!(field(r, "scripted_ttft_ns"), 50 * MS, "{r}");
    assert_eq!(array(r, "scripted_itl_ns"), vec![10 * MS; 3], "{r}");
    assert_eq!(field(r, "scripted_ttft_ns"), script.scripted_ttft_ns(0));
}

/// The end of a stream follows vLLM's server: the last token's chunk carries
/// `finish_reason`, and a chunk with `"choices": []` and `usage` follows only
/// when `stream_options.include_usage` is exactly `true`. Per-chunk usage
/// (`continuous_usage_stats`) is not sent.
#[tokio::test(flavor = "multi_thread")]
async fn stream_ends_like_a_vllm_server() {
    let stream = StreamSpec {
        ttft: Some(fixed(2)),
        itl: fixed(2),
        output_tokens: 3,
    };
    let script = script(1, vec![], stream);
    let h = launch(script.clone()).await;
    let cases = [
        (None, false),
        (Some(json!({"include_usage": true})), true),
        (Some(json!({"include_usage": false})), false),
        (Some(json!({"include_usage": "true"})), false),
        (Some(json!({})), false),
        (
            Some(json!({"include_usage": true, "continuous_usage_stats": true})),
            true,
        ),
    ];
    for endpoint in [CHAT, COMPLETIONS] {
        for (options, usage) in &cases {
            let mut body = match endpoint {
                CHAT => json!({"messages": [{"role": "user", "content": "hi"}], "stream": true}),
                _ => json!({"prompt": "hi", "stream": true}),
            };
            if let Some(options) = options {
                body["stream_options"] = options.clone();
            }
            let chunks = sse_chunks(&post_json(&h, endpoint, body).await);
            let case = format!("{endpoint} {options:?}: {chunks:?}");
            let role = usize::from(endpoint == CHAT);
            assert_eq!(chunks.len(), role + 3 + usize::from(*usage), "{case}");
            let tokens = &chunks[role..role + 3];
            for c in tokens {
                assert!(c.get("usage").is_none(), "{case}");
                assert_eq!(c["choices"].as_array().unwrap().len(), 1, "{case}");
            }
            for c in &tokens[..2] {
                assert!(c["choices"][0]["finish_reason"].is_null(), "{case}");
            }
            assert_eq!(tokens[2]["choices"][0]["finish_reason"], "length", "{case}");
            if *usage {
                let last = chunks.last().unwrap();
                assert_eq!(last["choices"], json!([]), "{case}");
                assert_eq!(last["id"], tokens[0]["id"], "{case}");
                assert_eq!(last["object"], tokens[0]["object"], "{case}");
                assert_eq!(last["model"], "phantom", "{case}");
                assert_eq!(field(&last["usage"], "completion_tokens"), 3, "{case}");
                assert_eq!(
                    field(&last["usage"], "total_tokens"),
                    field(&last["usage"], "prompt_tokens") + 3,
                    "{case}"
                );
            }
        }
    }
    let (_, mut reqs) = h.finish().await;
    assert_eq!(reqs.len(), 2 * cases.len());
    reqs.sort_by_key(|r| field(r, "seq"));
    for (i, r) in reqs.iter().enumerate() {
        let endpoint = if i < cases.len() { CHAT } else { COMPLETIONS };
        check_stream_line(r, &script, endpoint, 3);
    }
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
    refused(&["--role-chunk", "later"], "later");
    refused(&["--token-text", ""], "token text");
}
