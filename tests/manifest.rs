//! Proves `run` writes and flushes a complete manifest before it sends the
//! first request, for both runner kinds, and sends nothing if that write fails.

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
    Request { manifest_complete: bool },
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
    RunConfig { kind, interval: Duration::from_millis(10), count: COUNT, seed: Some(42) }
}

#[tokio::test(start_paused = true)]
async fn manifest_is_written_before_the_first_request() {
    for (kind, name) in [(RunnerKind::ClosedLoop, "closed_loop"), (RunnerKind::OpenLoop, "open_loop")] {
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
            Event::Request { manifest_complete: true },
            "{name}: first request saw a complete manifest"
        );

        let manifest: Value = serde_json::from_slice(&log.manifest).unwrap();
        assert_eq!(manifest["runner"], name);
        assert_eq!(manifest["interval_ns"], 10_000_000);
        assert_eq!(manifest["count"], COUNT);
        assert_eq!(manifest["seed"], 42);
        assert_eq!(manifest["paused_clock"], true);
        assert_eq!(manifest["tool_version"], env!("CARGO_PKG_VERSION"));
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
