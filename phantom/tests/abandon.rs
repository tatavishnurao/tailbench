//! Abandoned requests: a request whose client goes away, or whose handler is
//! dropped, before its response is finished still gets exactly one log line,
//! marked `"completed": false`, with `abandoned_ns` in place of `sent_ns`.
//! Completed lines carry no `completed` field.
//!
//! The streaming client is a raw TCP socket, so the test knows exactly how
//! many events it read before it closed the connection. ITLs are 50 ms, so
//! the server sees the close long before the next token is due.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use phantom::{Phantom, Script, ServiceSpec, Stall, StreamSpec};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;

const MS: u64 = 1_000_000;
const CHAT: &str = "/v1/chat/completions";
const COMPLETIONS: &str = "/v1/completions";

struct Harness {
    phantom: Phantom,
    addr: SocketAddr,
    log: PathBuf,
    _dir: TempDir,
}

async fn launch(script: Script) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("gt.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let phantom = phantom::start(listener, script, &log).await.unwrap();
    Harness {
        addr: phantom.local_addr(),
        phantom,
        log,
        _dir: dir,
    }
}

impl Harness {
    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Shuts down and returns the req lines, sorted by seq.
    async fn finish(self) -> Vec<Value> {
        self.phantom.shutdown().await.unwrap();
        let text = std::fs::read_to_string(&self.log).unwrap();
        let mut lines = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap());
        assert_eq!(lines.next().unwrap()["type"], "manifest");
        let mut reqs: Vec<Value> = lines.collect();
        reqs.sort_by_key(|r| field(r, "seq"));
        reqs
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

fn fixed(ms: u64) -> ServiceSpec {
    ServiceSpec::Fixed { ms }
}

fn stream_script(service_ms: u64, stalls: Vec<Stall>) -> Script {
    Script::new(1, fixed(service_ms), stalls)
        .unwrap()
        .with_stream(StreamSpec {
            ttft: Some(fixed(50)),
            itl: fixed(50),
            output_tokens: 8,
        })
        .unwrap()
}

/// Sends a streaming request on a raw socket, reads until `k` complete SSE
/// events have arrived, then closes the connection. Returns how many events
/// it had read when it closed: exactly `k` unless two arrived in one read.
fn stream_then_close(addr: SocketAddr, path: &str, body: &str, k: usize) -> usize {
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        sock,
        "POST {path} HTTP/1.1\r\nhost: phantom\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = sock.read(&mut buf).unwrap();
        assert!(n > 0, "server closed first: {}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&buf[..n]);
        // Each event is one `data: ...` line and a blank line; the chunked
        // transfer framing never contains `data: `.
        let text = String::from_utf8_lossy(&seen);
        let events = text.matches("data: ").count();
        if events >= k && text.ends_with("\n\n\r\n") {
            return events;
        }
    }
    // Dropping the socket closes the connection.
}

/// Checks a streamed abandoned line: no sent_ns, abandoned_ns after the last
/// hand-off, and every per-token array as long as the tokens handed off.
fn check_abandoned_stream(r: &Value, tokens: usize) {
    assert_eq!(r["completed"], false, "{r}");
    assert!(r.get("sent_ns").is_none(), "{r}");
    assert_eq!(r["stream"], true, "{r}");
    let token_ns = array(r, "token_ns");
    assert_eq!(token_ns.len(), tokens, "{r}");
    assert_eq!(array(r, "scheduled_token_ns").len(), tokens, "{r}");
    assert_eq!(array(r, "token_stall_wait_ns").len(), tokens, "{r}");
    assert_eq!(array(r, "scripted_itl_ns").len(), tokens.saturating_sub(1), "{r}");
    assert_eq!(
        field(r, "output_tokens"),
        8,
        "the scripted count, not the sent count: {r}"
    );
    let abandoned = field(r, "abandoned_ns");
    assert!(abandoned >= *token_ns.last().unwrap_or(&field(r, "arrival_ns")), "{r}");
    // Seen while waiting for the next token, before it was due.
    let next_due = field(r, "arrival_ns") + 50 * MS * (tokens as u64 + 1);
    assert!(
        abandoned < next_due,
        "close observed only after the next token was due: {r}"
    );
    assert_eq!(
        field(r, "scripted_service_ns"),
        field(r, "scripted_ttft_ns") + array(r, "scripted_itl_ns").iter().sum::<u64>(),
        "{r}"
    );
    assert_eq!(field(r, "stall_wait_ns"), 0, "{r}");
}

/// k token events on completions are k tokens handed off. Phantom records a
/// token when it hands the chunk to the response stream, and the client saw
/// all k, so all k were handed off; the (k+1)th was not, because the close
/// is seen while phantom waits for its scheduled time (50 ms away).
#[tokio::test(flavor = "multi_thread")]
async fn completions_stream_closed_after_k_events_logs_k_tokens() {
    let h = launch(stream_script(1, vec![])).await;
    let addr = h.addr;
    let body = json!({"prompt": "hi", "stream": true}).to_string();
    let read = tokio::task::spawn_blocking(move || stream_then_close(addr, COMPLETIONS, &body, 3))
        .await
        .unwrap();
    assert_eq!(read, 3, "precondition: closed after exactly 3 events");
    let reqs = h.finish().await;
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    let r = &reqs[0];
    check_abandoned_stream(r, 3);
    assert!(r.get(ROLE).is_none(), "{r}");
}

const ROLE: &str = "role_chunk_ns";

/// On chat the first event is the role chunk, so k events are k - 1 tokens.
#[tokio::test(flavor = "multi_thread")]
async fn chat_stream_closed_after_k_events_logs_k_minus_1_tokens() {
    let h = launch(stream_script(1, vec![])).await;
    let addr = h.addr;
    let body = json!({"messages": [], "stream": true}).to_string();
    let read = tokio::task::spawn_blocking(move || stream_then_close(addr, CHAT, &body, 3))
        .await
        .unwrap();
    assert_eq!(read, 3, "precondition: closed after exactly 3 events");
    let reqs = h.finish().await;
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    let r = &reqs[0];
    check_abandoned_stream(r, 2);
    assert!(field(r, ROLE) <= array(r, "token_ns")[0], "{r}");
}

/// A non-streaming client that gives up while its request is stalled.
#[tokio::test(flavor = "multi_thread")]
async fn non_streaming_timeout_during_a_stall_logs_one_abandoned_line() {
    // t0 is this request's arrival, so it lands at the start of the window.
    let stall = Stall {
        start_ms: 0,
        len_ms: 2_000,
    };
    let h = launch(stream_script(1, vec![stall])).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let err = client
        .post(format!("{}{CHAT}", h.base()))
        .header("x-request-id", "7")
        .json(&json!({"messages": []}))
        .send()
        .await
        .expect_err("precondition: the client timed out");
    assert!(err.is_timeout(), "{err}");
    let reqs = h.finish().await;
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    let r = &reqs[0];
    let expected: BTreeSet<&str> = [
        "type",
        "seq",
        "completed",
        "arrival_ns",
        "scripted_service_ns",
        "stall_wait_ns",
        "abandoned_ns",
        "client_id",
    ]
    .into();
    assert_eq!(keys(r), expected, "{r}");
    assert_eq!(r["completed"], false, "{r}");
    assert_eq!(field(r, "client_id"), 7, "{r}");
    assert_eq!(field(r, "scripted_service_ns"), MS, "{r}");
    let (arrival, wait, abandoned) = (
        field(r, "arrival_ns"),
        field(r, "stall_wait_ns"),
        field(r, "abandoned_ns"),
    );
    assert_eq!(wait, 2_000 * MS - arrival, "{r}");
    assert!(abandoned > arrival, "{r}");
    assert!(abandoned < arrival + wait, "drop not observed during the stall: {r}");
}

/// Abandoned and completed requests, streamed and not, interleaved: every
/// seq has exactly one line, and only the abandoned ones say `completed`.
#[tokio::test(flavor = "multi_thread")]
async fn seq_has_no_gaps_with_abandoned_requests() {
    // A 300 ms service time, so a client with a 50 ms timeout gives up mid-service.
    let h = launch(stream_script(300, vec![])).await;
    let base = h.base();
    let impatient = reqwest::Client::builder()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    let patient = reqwest::Client::new();
    let addr = h.addr;
    let plain = json!({"messages": []});
    let mut abandoned = BTreeSet::new();

    // seq 0: completed, non-streaming.
    patient
        .post(format!("{base}{CHAT}"))
        .json(&plain)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    // seq 1: abandoned, non-streaming.
    impatient
        .post(format!("{base}{CHAT}"))
        .json(&plain)
        .send()
        .await
        .expect_err("timed out");
    abandoned.insert(1);
    // seq 2: abandoned, streaming, after 2 events.
    let body = json!({"prompt": "hi", "stream": true}).to_string();
    let read = tokio::task::spawn_blocking(move || stream_then_close(addr, COMPLETIONS, &body, 2))
        .await
        .unwrap();
    assert_eq!(read, 2, "precondition");
    abandoned.insert(2);
    // seq 3: completed, streaming.
    patient
        .post(format!("{base}{COMPLETIONS}"))
        .json(&json!({"prompt": "hi", "stream": true}))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    // seq 4: abandoned, non-streaming; seq 5: completed, non-streaming.
    impatient
        .post(format!("{base}{COMPLETIONS}"))
        .json(&plain)
        .send()
        .await
        .expect_err("timed out");
    abandoned.insert(4);
    patient
        .post(format!("{base}{COMPLETIONS}"))
        .json(&plain)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    let reqs = h.finish().await;
    let seqs: Vec<u64> = reqs.iter().map(|r| field(r, "seq")).collect();
    assert_eq!(seqs, (0..6).collect::<Vec<_>>(), "one line per seq, no gaps");
    for r in &reqs {
        if abandoned.contains(&field(r, "seq")) {
            assert_eq!(r["completed"], false, "{r}");
            assert!(r.get("sent_ns").is_none() && r.get("abandoned_ns").is_some(), "{r}");
        } else {
            assert!(
                r.get("completed").is_none(),
                "completed lines have no completed field: {r}"
            );
            assert!(r.get("abandoned_ns").is_none() && r.get("sent_ns").is_some(), "{r}");
        }
    }
    check_abandoned_stream(&reqs[2], 2);
}
