//! phantom: a deterministic fake OpenAI-compatible server that logs ground
//! truth for every request.
//!
//! A load generator's numbers can only be audited against something that
//! knows what really happened. phantom follows a script fixed at startup:
//! each request's service time is a pure function of (seed, seq), and stall
//! windows are fixed offsets from the first request's arrival. It writes what
//! it did for every request to a JSONL log, which a harness's measurements
//! can then be checked against.
//!
//! Times in the log are nanoseconds since t0, the arrival of the first
//! non-healthz request, on the server's own monotonic clock.

use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const NS_PER_MS: u64 = 1_000_000;
/// The writer flushes anything it has written within this long.
const FLUSH_EVERY: Duration = Duration::from_millis(100);
/// Request bodies are read and discarded; this only bounds memory.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// How long a request is serviced for, after any stall.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ServiceSpec {
    /// The same whole number of milliseconds for every request.
    Fixed { ms: u64 },
    /// `median_ms * exp(sigma * Z)` with Z standard normal, drawn per request.
    Lognormal { median_ms: f64, sigma: f64 },
}

impl ServiceSpec {
    fn check(&self) -> Result<(), &'static str> {
        match *self {
            Self::Fixed { ms: 0 } => Err("milliseconds must be > 0"),
            Self::Fixed { ms } if ms.checked_mul(NS_PER_MS).is_none() => Err("too long to express in nanoseconds"),
            Self::Fixed { .. } => Ok(()),
            Self::Lognormal { median_ms, .. } if !(median_ms.is_finite() && median_ms > 0.0) => {
                Err("median must be a finite number of milliseconds > 0")
            }
            Self::Lognormal { sigma, .. } if !(sigma.is_finite() && sigma > 0.0) => Err("sigma must be finite and > 0"),
            Self::Lognormal { .. } => Ok(()),
        }
    }
}

/// A freeze: requests arriving in [start_ms, start_ms + len_ms) after t0 are
/// held until the window ends. Start inclusive, end exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Stall {
    pub start_ms: u64,
    pub len_ms: u64,
}

impl Stall {
    /// The window in ns since t0, checked to be non-empty and representable.
    fn window_ns(&self) -> Result<(u64, u64), &'static str> {
        if self.len_ms == 0 {
            return Err("length must be > 0 ms");
        }
        self.start_ms
            .checked_add(self.len_ms)
            .and_then(|end| end.checked_mul(NS_PER_MS))
            .map(|end_ns| (self.start_ms * NS_PER_MS, end_ns))
            .ok_or("window ends too late to express in nanoseconds")
    }
}

/// An invalid service or stall spec, with a message that says what and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError(String);

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SpecError {}

/// Parses `--service-ms`: a whole number of milliseconds > 0.
pub fn parse_service_ms(s: &str) -> Result<ServiceSpec, SpecError> {
    let invalid = |why: &str| SpecError(format!("invalid service time {s:?}: {why}"));
    let ms = s
        .parse::<u64>()
        .map_err(|_| invalid("expected a whole number of milliseconds > 0"))?;
    let spec = ServiceSpec::Fixed { ms };
    spec.check().map_err(invalid)?;
    Ok(spec)
}

/// Parses `--lognormal`: `MEDIAN_MS:SIGMA`, both finite and > 0.
pub fn parse_lognormal(s: &str) -> Result<ServiceSpec, SpecError> {
    let invalid = |why: &str| SpecError(format!("invalid lognormal spec {s:?}: {why}"));
    let [median, sigma] = split_pair(s).ok_or_else(|| invalid("expected MEDIAN_MS:SIGMA, e.g. 20:0.5"))?;
    let number = |part: &str| {
        part.parse::<f64>()
            .map_err(|_| invalid(&format!("{part:?} is not a number")))
    };
    let spec = ServiceSpec::Lognormal {
        median_ms: number(median)?,
        sigma: number(sigma)?,
    };
    spec.check().map_err(invalid)?;
    Ok(spec)
}

/// Parses `--stall`: `START_MS:LEN_MS`, whole milliseconds, length > 0.
pub fn parse_stall(s: &str) -> Result<Stall, SpecError> {
    let invalid = |why: &str| SpecError(format!("invalid stall {s:?}: {why}"));
    let [start, len] = split_pair(s).ok_or_else(|| invalid("expected START_MS:LEN_MS, e.g. 2000:500"))?;
    let whole = |part: &str| {
        part.parse::<u64>()
            .map_err(|_| invalid(&format!("{part:?} is not a whole number of milliseconds")))
    };
    let stall = Stall {
        start_ms: whole(start)?,
        len_ms: whole(len)?,
    };
    stall.window_ns().map_err(invalid)?;
    Ok(stall)
}

/// Splits `a:b` into exactly two parts.
fn split_pair(s: &str) -> Option<[&str; 2]> {
    let mut parts = s.split(':');
    let pair = [parts.next()?, parts.next()?];
    parts.next().is_none().then_some(pair)
}

/// What the server does to each request, fixed at startup.
#[derive(Debug, Clone)]
pub struct Script {
    seed: u64,
    service: ServiceSpec,
    /// Sorted by start, non-overlapping.
    stalls: Vec<Stall>,
    /// `stalls` as [start, end) in ns since t0.
    windows_ns: Vec<(u64, u64)>,
}

impl Script {
    /// Validates the specs (they may not have come through the parsers) and
    /// refuses overlapping stalls, whose combined wait would be ambiguous.
    /// Windows that only touch are allowed. Stalls are kept sorted by start.
    pub fn new(seed: u64, service: ServiceSpec, mut stalls: Vec<Stall>) -> Result<Self, SpecError> {
        service
            .check()
            .map_err(|why| SpecError(format!("invalid service spec {service:?}: {why}")))?;
        stalls.sort_by_key(|s| s.start_ms);
        let windows_ns = stalls
            .iter()
            .map(|s| {
                s.window_ns()
                    .map_err(|why| SpecError(format!("invalid stall {}:{}: {why}", s.start_ms, s.len_ms)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (i, pair) in windows_ns.windows(2).enumerate() {
            if pair[1].0 < pair[0].1 {
                let (a, b) = (stalls[i], stalls[i + 1]);
                return Err(SpecError(format!(
                    "stalls {}:{} and {}:{} overlap",
                    a.start_ms, a.len_ms, b.start_ms, b.len_ms
                )));
            }
        }
        Ok(Self {
            seed,
            service,
            stalls,
            windows_ns,
        })
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn service(&self) -> ServiceSpec {
        self.service
    }

    pub fn stalls(&self) -> &[Stall] {
        &self.stalls
    }

    /// Service time for request `seq`: a pure function of (seed, seq).
    pub fn scripted_service_ns(&self, seq: u64) -> u64 {
        match self.service {
            ServiceSpec::Fixed { ms } => ms * NS_PER_MS,
            ServiceSpec::Lognormal { median_ms, sigma } => {
                let z = standard_normal(self.seed, seq);
                // `as` saturates, so an extreme draw is capped, not wrapped.
                (median_ms * NS_PER_MS as f64 * (sigma * z).exp()).round() as u64
            }
        }
    }

    /// How long a request arriving at `arrival_ns` waits before service: until
    /// the end of the stall window containing it, or 0 outside every window.
    pub fn stall_wait_ns(&self, arrival_ns: u64) -> u64 {
        self.windows_ns
            .iter()
            .find(|&&(start, end)| start <= arrival_ns && arrival_ns < end)
            .map_or(0, |&(_, end)| end - arrival_ns)
    }
}

/// splitmix64 (Steele, Lea and Flood, 2014): a state advanced by a fixed odd
/// increment and a bijective output mix. Small and self-contained, and well
/// distributed enough to script service times.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A standard normal draw for request `seq`, by Box-Muller from two uniforms.
/// Each request's stream starts at mix(seed) XOR seq: nearby seqs start at
/// nearby states, but the increment is far larger than their distance, so
/// requests do not share draws.
fn standard_normal(seed: u64, seq: u64) -> f64 {
    let mut rng = SplitMix64(SplitMix64(seed).next() ^ seq);
    // 53 random bits each. u1 is in (0, 1] so its logarithm is finite.
    let scale = 1.0 / (1u64 << 53) as f64;
    let u1 = ((rng.next() >> 11) + 1) as f64 * scale;
    let u2 = (rng.next() >> 11) as f64 * scale;
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// First line of the log.
#[derive(Serialize)]
#[serde(tag = "type", rename = "manifest")]
struct Manifest<'a> {
    phantom_version: &'static str,
    seed: u64,
    service: ServiceSpec,
    stalls: &'a [Stall],
    listen: String,
    /// Wall-clock start, for matching runs up; never used for intervals.
    start_unix_ns: u64,
    times: &'static str,
}

/// One line per chat request; all times are ns since t0.
#[derive(Serialize)]
#[serde(tag = "type", rename = "req")]
struct Record {
    seq: u64,
    arrival_ns: u64,
    scripted_service_ns: u64,
    stall_wait_ns: u64,
    sent_ns: u64,
}

struct Shared {
    script: Script,
    next_seq: AtomicU64,
    /// Arrival of the first chat request; health checks do not set it.
    t0: OnceLock<Instant>,
    log: mpsc::Sender<Record>,
}

/// A running server. Dropping it without `shutdown` leaves the log unflushed.
pub struct Phantom {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    server: JoinHandle<io::Result<()>>,
    writer: thread::JoinHandle<io::Result<()>>,
}

/// Writes the manifest, then serves on `listener` and logs to `log_path`
/// (created, or truncated) until `shutdown`. Nothing is served if the
/// manifest cannot be written.
pub async fn start(listener: TcpListener, script: Script, log_path: &Path) -> io::Result<Phantom> {
    let addr = listener.local_addr()?;
    let mut out = BufWriter::new(File::create(log_path)?);
    let manifest = Manifest {
        phantom_version: VERSION,
        seed: script.seed,
        service: script.service,
        stalls: &script.stalls,
        listen: addr.to_string(),
        start_unix_ns: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, nanos),
        times: "ns since t0, the arrival of the first non-healthz request, on the server's monotonic clock",
    };
    serde_json::to_writer(&mut out, &manifest)?;
    out.write_all(b"\n")?;
    out.flush()?;

    // A dedicated thread does the file I/O, so handlers only pay for a
    // channel send.
    let (tx, rx) = mpsc::channel();
    let writer = thread::Builder::new()
        .name("phantom-log".to_owned())
        .spawn(move || write_records(out, rx))?;

    let shared = Arc::new(Shared {
        script,
        next_seq: AtomicU64::new(0),
        t0: OnceLock::new(),
        log: tx,
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/healthz", get(healthz))
        .with_state(shared);
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                // A dropped sender also stops the server.
                let _ = stopped.await;
            })
            .await
    });
    Ok(Phantom {
        addr,
        stop,
        server,
        writer,
    })
}

impl Phantom {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting connections, waits for in-flight requests to be
    /// answered, then flushes and closes the log. Returns the first error from
    /// serving or from writing the log.
    pub async fn shutdown(self) -> io::Result<()> {
        let _ = self.stop.send(());
        let served = self.server.await.map_err(io::Error::other)?;
        // Once serving has finished, the router and every connection are gone,
        // and with them every sender: the writer drains, flushes and returns.
        let writer = self.writer;
        let written = tokio::task::spawn_blocking(move || writer.join())
            .await
            .map_err(io::Error::other)?
            .map_err(|_| io::Error::other("log writer panicked"))?;
        served.and(written)
    }
}

/// Writes records as they come, flushing anything written within
/// `FLUSH_EVERY`, and once more when every sender has gone.
fn write_records(mut out: BufWriter<File>, rx: Receiver<Record>) -> io::Result<()> {
    // When the oldest unflushed record was written; None when all is flushed.
    let mut unflushed_since: Option<std::time::Instant> = None;
    loop {
        let next = match unflushed_since {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(since) => rx.recv_timeout((since + FLUSH_EVERY).saturating_duration_since(std::time::Instant::now())),
        };
        match next {
            Ok(record) => {
                serde_json::to_writer(&mut out, &record)?;
                out.write_all(b"\n")?;
                unflushed_since.get_or_insert_with(std::time::Instant::now);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return out.flush(),
        }
        if unflushed_since.is_some_and(|since| since.elapsed() >= FLUSH_EVERY) {
            out.flush()?;
            unflushed_since = None;
        }
    }
}

async fn chat_completions(State(shared): State<Arc<Shared>>, body: Body) -> Response {
    let seq = shared.next_seq.fetch_add(1, Ordering::Relaxed);
    // t0 is read before arrival, so arrival_ns is never negative.
    let t0 = *shared.t0.get_or_init(Instant::now);
    let arrival = Instant::now();
    // Read so the connection can be reused; the content does not matter.
    let _ = axum::body::to_bytes(body, MAX_BODY_BYTES).await;

    let script = &shared.script;
    let arrival_ns = nanos(arrival - t0);
    let stall_wait_ns = script.stall_wait_ns(arrival_ns);
    let scripted_service_ns = script.scripted_service_ns(seq);
    // Held until the window ends, measured from arrival, however long the
    // body took; then serviced.
    sleep_until(arrival + Duration::from_nanos(stall_wait_ns)).await;
    sleep(Duration::from_nanos(scripted_service_ns)).await;

    let response = Json(completion(seq)).into_response();
    let sent_ns = nanos(t0.elapsed());
    // Only fails if the writer has stopped on an I/O error, which `shutdown`
    // reports.
    let _ = shared.log.send(Record {
        seq,
        arrival_ns,
        scripted_service_ns,
        stall_wait_ns,
        sent_ns,
    });
    response
}

async fn healthz() -> &'static str {
    "ok"
}

/// A minimal non-streaming chat completion. It depends only on seq: `created`
/// is fixed rather than the time of the request.
fn completion(seq: u64) -> Value {
    json!({
        "id": format!("chatcmpl-phantom-{seq}"),
        "object": "chat.completion",
        "created": 0,
        "model": "phantom",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "This is a phantom response."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 8, "completion_tokens": 6, "total_tokens": 14}
    })
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}
