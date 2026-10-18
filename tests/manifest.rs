//! Proves `run` writes and flushes a complete manifest before it sends the
//! first request, for both runner kinds, and sends nothing if that write fails.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tailbench::{RunConfig, RunnerKind, run};

#[derive(Debug, PartialEq)]
enum Event {
    ManifestFlushed,
    /// A request was sent; `manifest_complete` is whether the bytes written
    /// so far parsed as a whole JSON document at that moment.
    Request {
        manifest_complete: bool,
    },
}

#[derive(Default)]
struct Log {
    manifest: Vec<u8>,
    events: Vec<Event>,
}

/// Manifest sink that logs a flush as an event, sharing the log with the server.
struct LogWriter(Arc<Mutex<Log>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().manifest.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().events.push(Event::ManifestFlushed);
        Ok(())
    }
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("disk full"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const COUNT: u64 = 3;

fn config(kind: RunnerKind) -> RunConfig {
    RunConfig {
        kind,
        interval: Duration::from_millis(10),
        count: COUNT,
        seed: Some(42),
        extra: BTreeMap::new(),
    }
}

/// Runs `config` against a fake server that sleeps `service` per request and
/// returns the manifest it wrote, parsed and as text.
async fn manifest_of(config: &RunConfig, service: Duration) -> (Value, String) {
    let mut out = Vec::new();
    run(config, &mut out, move || tokio::time::sleep(service))
        .await
        .expect("run succeeds");
    let text = String::from_utf8(out).expect("manifest is UTF-8");
    (serde_json::from_str(&text).expect("manifest is JSON"), text)
}

#[tokio::test(start_paused = true)]
async fn manifest_records_current_thread_runtime() {
    for kind in [RunnerKind::ClosedLoop, RunnerKind::OpenLoop] {
        let (manifest, _) = manifest_of(&config(kind), Duration::from_millis(1)).await;
        assert_eq!(manifest["runtime_flavor"], "current_thread", "{kind:?}");
        // tokio documents num_workers as always 1 on current_thread.
        assert_eq!(manifest["runtime_workers"], 1, "{kind:?}");
    }
}

// start_paused needs current_thread, so this one runs on the real clock: a
// few sends, 1 ms apart, to a server that answers immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_records_multi_thread_runtime() {
    for kind in [RunnerKind::ClosedLoop, RunnerKind::OpenLoop] {
        let config = RunConfig {
            interval: Duration::from_millis(1),
            ..config(kind)
        };
        let (manifest, _) = manifest_of(&config, Duration::ZERO).await;
        assert_eq!(manifest["runtime_flavor"], "multi_thread", "{kind:?}");
        assert_eq!(manifest["runtime_workers"], 2, "{kind:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn manifest_records_build_profile() {
    let (manifest, _) = manifest_of(&config(RunnerKind::OpenLoop), Duration::from_millis(1)).await;
    // `cargo test` builds with the test profile, which inherits from dev.
    assert_eq!(manifest["build_profile"], "debug");
    let opt_level = manifest["opt_level"].as_str().expect("opt_level is a string");
    assert_ne!(opt_level, "UNKNOWN");
}

#[tokio::test(start_paused = true)]
async fn manifest_extra_has_sorted_keys_and_defaults_to_empty() {
    let (manifest, _) = manifest_of(&config(RunnerKind::OpenLoop), Duration::from_millis(1)).await;
    assert_eq!(manifest["extra"], serde_json::json!({}), "empty map is an empty object");

    let mut labeled = config(RunnerKind::OpenLoop);
    // Inserted out of order: the manifest must list them sorted.
    labeled.extra.insert("target".to_owned(), "fake-server".to_owned());
    labeled.extra.insert("gpu".to_owned(), "4060".to_owned());
    let (manifest, text) = manifest_of(&labeled, Duration::from_millis(1)).await;
    assert_eq!(
        manifest["extra"],
        serde_json::json!({"gpu": "4060", "target": "fake-server"})
    );
    // serde_json::Value sorts keys itself, so check the order in the raw text.
    let gpu = text.find("\"gpu\"").expect("gpu key in manifest text");
    let target = text.find("\"target\"").expect("target key in manifest text");
    assert!(gpu < target, "extra keys not sorted:\n{text}");
}

#[tokio::test(start_paused = true)]
async fn manifest_is_written_before_the_first_request() {
    for (kind, name) in [
        (RunnerKind::ClosedLoop, "closed_loop"),
        (RunnerKind::OpenLoop, "open_loop"),
    ] {
        let log = Arc::new(Mutex::new(Log::default()));
        let server_log = Arc::clone(&log);
        let result = run(&config(kind), LogWriter(Arc::clone(&log)), move || {
            // Runs at send time, before the request future is awaited.
            let mut log = server_log.lock().unwrap();
            let manifest_complete = serde_json::from_slice::<Value>(&log.manifest).is_ok();
            log.events.push(Event::Request { manifest_complete });
            tokio::time::sleep(Duration::from_millis(1))
        })
        .await
        .expect("run succeeds");

        let log = log.lock().unwrap();
        assert_eq!(result.latency.hist.len(), COUNT, "{name}: every request recorded");
        assert_eq!(log.events.len(), 1 + COUNT as usize, "{name}: {:?}", log.events);
        assert_eq!(log.events[0], Event::ManifestFlushed, "{name}: manifest flushed first");
        assert_eq!(
            log.events[1],
            Event::Request {
                manifest_complete: true
            },
            "{name}: first request saw a complete manifest"
        );

        let manifest: Value = serde_json::from_slice(&log.manifest).unwrap();
        assert_eq!(manifest["runner"], name);
        assert_eq!(manifest["interval_ns"], 10_000_000);
        assert_eq!(manifest["count"], COUNT);
        assert_eq!(manifest["seed"], 42);
        assert_eq!(manifest["paused_clock"], true);
        assert_eq!(manifest["tool_version"], env!("CARGO_PKG_VERSION"));
        // Values depend on the checkout, so only presence is checked here.
        for key in [
            "build_git_sha",
            "build_git_dirty",
            "runtime_git_sha",
            "runtime_git_dirty",
            "sha_mismatch",
        ] {
            assert!(manifest.get(key).is_some(), "{name}: manifest has {key}");
        }
        for old_key in ["git_sha", "git_dirty"] {
            assert!(manifest.get(old_key).is_none(), "{name}: {old_key} was renamed");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn failed_manifest_write_sends_nothing() {
    let sent = Arc::new(Mutex::new(0u64));
    let counter = Arc::clone(&sent);
    let result = run(&config(RunnerKind::OpenLoop), FailingWriter, move || {
        *counter.lock().unwrap() += 1;
        tokio::time::sleep(Duration::from_millis(1))
    })
    .await;

    assert!(result.is_err(), "manifest write error is returned");
    assert_eq!(*sent.lock().unwrap(), 0, "no request sent without a manifest");
}
