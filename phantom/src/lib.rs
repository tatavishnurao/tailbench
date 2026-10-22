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
//! A request with `"stream": true` is answered as Server-Sent Events, one
//! chunk per token, on a scripted time to first token (TTFT) and inter-token
//! latency (ITL); its log line also records when each token was scheduled
//! and when it was handed off.
//!
//! A request whose client goes away before its response is finished still
//! gets a line, marked `"completed": false`.
//!
//! Times in the log are nanoseconds since t0, the arrival of the first chat
//! or completions request, on the server's own monotonic clock.

use std::convert::Infallible;
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
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Tokens per streamed response when neither `--output-tokens` nor the
/// request says otherwise.
pub const DEFAULT_OUTPUT_TOKENS: u64 = 32;
/// ITL when `--itl-ms` and `--itl-lognormal` are both unset.
pub const DEFAULT_ITL: ServiceSpec = ServiceSpec::Fixed { ms: 10 };
/// The text of every streamed token unless `--token-text` says otherwise:
/// one chunk is one token.
pub const DEFAULT_TOKEN_TEXT: &str = " tok";

const NS_PER_MS: u64 = 1_000_000;
const PROMPT_TOKENS: u64 = 8;

/// Keys for the TTFT and ITL draws, so they never reuse the service-time
/// draw or each other's ("ttft" and "itl" in ASCII).
const TTFT_DOMAIN: u64 = 0x7474_6674;
const ITL_DOMAIN: u64 = 0x0069_746c;
/// The writer flushes anything it has written within this long.
const FLUSH_EVERY: Duration = Duration::from_millis(100);
/// Request bodies are read and discarded; this only bounds memory.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// A client's own id for a request, logged as `client_id` if it is a u64.
const REQUEST_ID_HEADER: &str = "x-request-id";

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
    parse_fixed_ms("service time", s)
}

/// Parses `--lognormal`: `MEDIAN_MS:SIGMA`, both finite and > 0.
pub fn parse_lognormal(s: &str) -> Result<ServiceSpec, SpecError> {
    parse_lognormal_spec("lognormal spec", s)
}

/// Parses `--ttft-ms`, like `--service-ms`.
pub fn parse_ttft_ms(s: &str) -> Result<ServiceSpec, SpecError> {
    parse_fixed_ms("TTFT", s)
}

/// Parses `--ttft-lognormal`, like `--lognormal`.
pub fn parse_ttft_lognormal(s: &str) -> Result<ServiceSpec, SpecError> {
    parse_lognormal_spec("TTFT lognormal spec", s)
}

/// Parses `--itl-ms`, like `--service-ms`.
pub fn parse_itl_ms(s: &str) -> Result<ServiceSpec, SpecError> {
    parse_fixed_ms("ITL", s)
}

/// Parses `--itl-lognormal`, like `--lognormal`.
pub fn parse_itl_lognormal(s: &str) -> Result<ServiceSpec, SpecError> {
    parse_lognormal_spec("ITL lognormal spec", s)
}

/// Parses `--output-tokens`: a whole number > 0.
pub fn parse_output_tokens(s: &str) -> Result<u64, SpecError> {
    s.parse::<u64>()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| SpecError(format!("invalid output tokens {s:?}: expected a whole number > 0")))
}

/// A whole number of milliseconds > 0; `what` names it in the error.
fn parse_fixed_ms(what: &str, s: &str) -> Result<ServiceSpec, SpecError> {
    let invalid = |why: &str| SpecError(format!("invalid {what} {s:?}: {why}"));
    let ms = s
        .parse::<u64>()
        .map_err(|_| invalid("expected a whole number of milliseconds > 0"))?;
    let spec = ServiceSpec::Fixed { ms };
    spec.check().map_err(invalid)?;
    Ok(spec)
}

/// `MEDIAN_MS:SIGMA`, both finite and > 0; `what` names it in the error.
fn parse_lognormal_spec(what: &str, s: &str) -> Result<ServiceSpec, SpecError> {
    let invalid = |why: &str| SpecError(format!("invalid {what} {s:?}: {why}"));
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

/// Parses `--token-text`: any text but the empty string, which a client
/// would not count as a token.
pub fn parse_token_text(s: &str) -> Result<String, SpecError> {
    if s.is_empty() {
        return Err(SpecError(
            "invalid token text \"\": expected at least one character".to_owned(),
        ));
    }
    Ok(s.to_owned())
}

/// Splits `a:b` into exactly two parts.
fn split_pair(s: &str) -> Option<[&str; 2]> {
    let mut parts = s.split(':');
    let pair = [parts.next()?, parts.next()?];
    parts.next().is_none().then_some(pair)
}

/// When a chat stream's role chunk is handed off. Completions streams have
/// no role chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoleChunk {
    /// Just before token 0, so the first chunk and the first content chunk
    /// arrive together.
    #[default]
    WithFirstToken,
    /// As soon as the request arrives, before TTFT.
    AtArrival,
}

/// Parses `--role-chunk`: `with-first-token` or `at-arrival`.
pub fn parse_role_chunk(s: &str) -> Result<RoleChunk, SpecError> {
    match s {
        "with-first-token" => Ok(RoleChunk::WithFirstToken),
        "at-arrival" => Ok(RoleChunk::AtArrival),
        _ => Err(SpecError(format!(
            "invalid role chunk {s:?}: expected with-first-token or at-arrival"
        ))),
    }
}

/// How streamed responses are timed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamSpec {
    /// Time to first token; `None` uses the script's service-time spec (with
    /// its own draws, not the service-time ones).
    pub ttft: Option<ServiceSpec>,
    /// Gap before each token after the first.
    pub itl: ServiceSpec,
    /// Tokens per response, unless the request's token limit
    /// (`max_completion_tokens`, else `max_tokens`) is smaller.
    pub output_tokens: u64,
}

impl Default for StreamSpec {
    fn default() -> Self {
        Self {
            ttft: None,
            itl: DEFAULT_ITL,
            output_tokens: DEFAULT_OUTPUT_TOKENS,
        }
    }
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
    stream: StreamSpec,
    role_chunk: RoleChunk,
    token_text: String,
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
            stream: StreamSpec::default(),
            role_chunk: RoleChunk::default(),
            token_text: DEFAULT_TOKEN_TEXT.to_owned(),
        })
    }

    /// Sets how streamed responses are timed (default: `StreamSpec::default()`),
    /// validating it like the other specs.
    pub fn with_stream(mut self, stream: StreamSpec) -> Result<Self, SpecError> {
        if let Some(ttft) = stream.ttft {
            ttft.check()
                .map_err(|why| SpecError(format!("invalid TTFT spec {ttft:?}: {why}")))?;
        }
        let itl = stream.itl;
        itl.check()
            .map_err(|why| SpecError(format!("invalid ITL spec {itl:?}: {why}")))?;
        if stream.output_tokens == 0 {
            return Err(SpecError(
                "invalid output tokens 0: expected a whole number > 0".to_owned(),
            ));
        }
        self.stream = stream;
        Ok(self)
    }

    /// Sets when a chat stream's role chunk is handed off (default:
    /// `RoleChunk::WithFirstToken`).
    pub fn with_role_chunk(mut self, role_chunk: RoleChunk) -> Self {
        self.role_chunk = role_chunk;
        self
    }

    /// Sets the text every streamed token carries (default:
    /// `DEFAULT_TOKEN_TEXT`), refusing the empty string like the parser.
    pub fn with_token_text(mut self, text: &str) -> Result<Self, SpecError> {
        self.token_text = parse_token_text(text)?;
        Ok(self)
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

    pub fn stream(&self) -> StreamSpec {
        self.stream
    }

    pub fn role_chunk(&self) -> RoleChunk {
        self.role_chunk
    }

    pub fn token_text(&self) -> &str {
        &self.token_text
    }

    /// The TTFT spec in effect: `--ttft-*` if given, else the service spec.
    pub fn ttft_spec(&self) -> ServiceSpec {
        self.stream.ttft.unwrap_or(self.service)
    }

    /// Service time for request `seq`: a pure function of (seed, seq).
    pub fn scripted_service_ns(&self, seq: u64) -> u64 {
        sample_ns(self.service, || standard_normal(self.seed, seq))
    }

    /// Time to first token for streamed request `seq`: a pure function of
    /// (seed, seq), drawn from its own stream.
    pub fn scripted_ttft_ns(&self, seq: u64) -> u64 {
        sample_ns(self.ttft_spec(), || keyed_normal(self.seed, TTFT_DOMAIN, seq, 0))
    }

    /// Gap before token `index` (1 for the second token) of streamed request
    /// `seq`: a pure function of (seed, seq, index), drawn from its own stream.
    pub fn scripted_itl_ns(&self, seq: u64, index: u64) -> u64 {
        sample_ns(self.stream.itl, || keyed_normal(self.seed, ITL_DOMAIN, seq, index))
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

/// One splitmix64 output from state `x`: a bijective mix of all 64 bits.
fn mix(x: u64) -> u64 {
    SplitMix64(x).next()
}

/// `spec` in ns; `z` is a standard normal draw, taken only for lognormal.
fn sample_ns(spec: ServiceSpec, z: impl FnOnce() -> f64) -> u64 {
    match spec {
        ServiceSpec::Fixed { ms } => ms * NS_PER_MS,
        // `as` saturates, so an extreme draw is capped, not wrapped.
        ServiceSpec::Lognormal { median_ms, sigma } => {
            (median_ms * NS_PER_MS as f64 * (sigma * z()).exp()).round() as u64
        }
    }
}

/// A standard normal draw for request `seq`, by Box-Muller from two uniforms.
/// Each request's stream starts at mix(seed) XOR seq: nearby seqs start at
/// nearby states, but the increment is far larger than their distance, so
/// requests do not share draws.
fn standard_normal(seed: u64, seq: u64) -> f64 {
    normal_from(mix(seed) ^ seq)
}

/// A standard normal draw for (seq, index) in `domain` (TTFT or ITL). The
/// start state mixes seed, domain, seq and index one after another; mix is a
/// bijection, so a different domain (or a domain at all, against the service
/// draw's mix(seed)) gives a different stream.
fn keyed_normal(seed: u64, domain: u64, seq: u64, index: u64) -> f64 {
    normal_from(mix(mix(mix(seed ^ domain) ^ seq) ^ index))
}

/// Box-Muller from the first two outputs of splitmix64 started at `start`.
fn normal_from(start: u64) -> f64 {
    let mut rng = SplitMix64(start);
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
    /// The TTFT spec in effect (the service spec unless set).
    ttft: ServiceSpec,
    itl: ServiceSpec,
    output_tokens: u64,
    /// When a chat stream's role chunk is sent.
    role_chunk: RoleChunk,
    /// The text of every streamed token.
    token_text: &'a str,
    listen: String,
    /// Wall-clock start, for matching runs up; never used for intervals.
    start_unix_ns: u64,
    times: &'static str,
}

/// One line per chat or completions request; all times are ns since t0.
///
/// A completed request has every field but `completed` and `abandoned_ns`,
/// so its line is what it has always been. An abandoned one has
/// `"completed": false`, `abandoned_ns` instead of `sent_ns`, and whatever
/// else was known when it was dropped.
#[derive(Serialize)]
#[serde(tag = "type", rename = "req")]
struct Record {
    seq: u64,
    /// Abandoned lines only, and then always false.
    #[serde(skip_serializing_if = "Option::is_none")]
    completed: Option<bool>,
    arrival_ns: u64,
    /// For a stream: scripted_ttft_ns + the sum of scripted_itl_ns. Omitted
    /// only if the request was dropped before its body was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    scripted_service_ns: Option<u64>,
    /// For a stream: the sum of token_stall_wait_ns. Omitted like
    /// scripted_service_ns.
    #[serde(skip_serializing_if = "Option::is_none")]
    stall_wait_ns: Option<u64>,
    /// For a stream: when the [DONE] event was handed off. Completed only.
    #[serde(skip_serializing_if = "Option::is_none")]
    sent_ns: Option<u64>,
    /// When the drop was observed. Abandoned only.
    #[serde(skip_serializing_if = "Option::is_none")]
    abandoned_ns: Option<u64>,
    /// The request's `x-request-id` header, if it is a u64; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<u64>,
    /// Streams only; a non-streaming line has none of these fields.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    stream: Option<StreamRecord>,
}

/// The per-token ground truth of one streamed response.
#[derive(Serialize)]
struct StreamRecord {
    stream: bool,
    endpoint: &'static str,
    scripted_ttft_ns: u64,
    /// The scripted gap before each token after the first.
    scripted_itl_ns: Vec<u64>,
    /// When each token's chunk was handed to the response stream; on an
    /// abandoned line, only the tokens handed off before the drop, and every
    /// other per-token array matches it.
    token_ns: Vec<u64>,
    /// When each token was scheduled, after any stall shift.
    scheduled_token_ns: Vec<u64>,
    /// How far a stall shifted each token's schedule (and every later one).
    token_stall_wait_ns: Vec<u64>,
    /// When the chat role chunk was handed off; omitted for completions.
    #[serde(skip_serializing_if = "Option::is_none")]
    role_chunk_ns: Option<u64>,
    /// Tokens the response was scripted to carry: on a completed line, the
    /// tokens sent.
    output_tokens: u64,
}

/// A request's log line, written exactly once: by `finish` once the
/// response is complete, or else on drop, as abandoned. A handler's future is
/// dropped when its client disconnects, and a stream's task stops when the
/// client stops reading; either way the line is written with what was known
/// by then.
struct LogLine {
    shared: Arc<Shared>,
    t0: Instant,
    /// Some until written.
    record: Option<Record>,
}

impl LogLine {
    fn record(&mut self) -> &mut Record {
        self.record.as_mut().expect("a line is written only by finish or drop")
    }

    fn stream(&mut self) -> &mut StreamRecord {
        self.record().stream.as_mut().expect("a streamed request's line")
    }

    fn finish(mut self, sent_ns: u64) {
        if let Some(mut record) = self.record.take() {
            record.sent_ns = Some(sent_ns);
            self.write(record);
        }
    }

    /// Only fails if the writer has stopped on an I/O error, which `shutdown`
    /// reports.
    fn write(&self, record: Record) {
        let _ = self.shared.log.send(record);
    }
}

impl Drop for LogLine {
    fn drop(&mut self) {
        if let Some(mut record) = self.record.take() {
            record.completed = Some(false);
            record.abandoned_ns = Some(nanos(self.t0.elapsed()));
            self.write(record);
        }
    }
}

struct Shared {
    script: Script,
    next_seq: AtomicU64,
    /// Arrival of the first chat or completions request; health checks do
    /// not set it.
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
        ttft: script.ttft_spec(),
        itl: script.stream.itl,
        output_tokens: script.stream.output_tokens,
        role_chunk: script.role_chunk,
        token_text: &script.token_text,
        listen: addr.to_string(),
        start_unix_ns: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, nanos),
        times: "ns since t0, the arrival of the first chat or completions request, on the server's monotonic clock",
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
        .route("/v1/completions", post(completions))
        .route("/healthz", get(healthz))
        .route("/health", get(healthz))
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

/// Which OpenAI endpoint a request came in on; it decides only the shape of
/// the response, never its timing.
#[derive(Debug, Clone, Copy)]
enum Endpoint {
    Chat,
    Completions,
}

impl Endpoint {
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Completions => "/v1/completions",
        }
    }

    fn id(self, seq: u64) -> String {
        match self {
            Self::Chat => format!("chatcmpl-phantom-{seq}"),
            Self::Completions => format!("cmpl-phantom-{seq}"),
        }
    }

    /// A minimal non-streaming response. It depends only on seq: `created`
    /// is fixed rather than the time of the request.
    fn completion(self, seq: u64) -> Value {
        let text = "This is a phantom response.";
        let usage = usage(6);
        match self {
            Self::Chat => json!({
                "id": self.id(seq),
                "object": "chat.completion",
                "created": 0,
                "model": "phantom",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": text},
                    "finish_reason": "stop"
                }],
                "usage": usage
            }),
            Self::Completions => json!({
                "id": self.id(seq),
                "object": "text_completion",
                "created": 0,
                "model": "phantom",
                "choices": [{"index": 0, "text": text, "logprobs": null, "finish_reason": "stop"}],
                "usage": usage
            }),
        }
    }

    /// One streamed chunk with one choice: `delta` is the chat delta, `text`
    /// the completions text.
    fn chunk(self, seq: u64, delta: Value, text: &str, finish_reason: Option<&str>) -> Value {
        let choice = match self {
            Self::Chat => json!({"index": 0, "delta": delta, "finish_reason": finish_reason}),
            Self::Completions => json!({"index": 0, "text": text, "logprobs": null, "finish_reason": finish_reason}),
        };
        self.stream_chunk(seq, json!([choice]))
    }

    fn stream_chunk(self, seq: u64, choices: Value) -> Value {
        let object = match self {
            Self::Chat => "chat.completion.chunk",
            Self::Completions => "text_completion",
        };
        json!({
            "id": self.id(seq),
            "object": object,
            "created": 0,
            "model": "phantom",
            "choices": choices
        })
    }

    /// The chunk that opens a chat stream; completions have none.
    fn role_chunk(self, seq: u64) -> Option<Value> {
        match self {
            Self::Chat => Some(self.chunk(seq, json!({"role": "assistant", "content": ""}), "", None)),
            Self::Completions => None,
        }
    }

    /// The last token's chunk carries `finish_reason`, as vLLM's does.
    fn token_chunk(self, seq: u64, text: &str, last: bool) -> Value {
        self.chunk(seq, json!({"content": text}), text, last.then_some("length"))
    }

    /// vLLM's final usage chunk: no choices, sent after the last token only
    /// when the request asked for it.
    fn usage_chunk(self, seq: u64, tokens: u64) -> Value {
        let mut chunk = self.stream_chunk(seq, json!([]));
        chunk["usage"] = usage(tokens);
        chunk
    }
}

fn usage(completion_tokens: u64) -> Value {
    json!({
        "prompt_tokens": PROMPT_TOKENS,
        "completion_tokens": completion_tokens,
        "total_tokens": PROMPT_TOKENS + completion_tokens
    })
}

/// The only parts of a request body phantom reads.
struct RequestOptions {
    /// `"stream": true`, exactly; anything else is a non-streaming request.
    stream: bool,
    /// `max_completion_tokens` if it is a whole number, else `max_tokens` if
    /// that is.
    max_tokens: Option<u64>,
    /// `"stream_options": {"include_usage": true}`, exactly: the stream ends
    /// with a usage chunk.
    include_usage: bool,
}

impl RequestOptions {
    /// Anything that is not a JSON object reads as a non-streaming request.
    fn parse(body: &[u8]) -> Self {
        let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        Self {
            stream: value.get("stream") == Some(&Value::Bool(true)),
            max_tokens: value
                .get("max_completion_tokens")
                .and_then(Value::as_u64)
                .or_else(|| value.get("max_tokens").and_then(Value::as_u64)),
            include_usage: value.pointer("/stream_options/include_usage") == Some(&Value::Bool(true)),
        }
    }
}

async fn chat_completions(state: State<Arc<Shared>>, headers: HeaderMap, body: Body) -> Response {
    handle(state, headers, body, Endpoint::Chat).await
}

async fn completions(state: State<Arc<Shared>>, headers: HeaderMap, body: Body) -> Response {
    handle(state, headers, body, Endpoint::Completions).await
}

async fn handle(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Body, endpoint: Endpoint) -> Response {
    let seq = shared.next_seq.fetch_add(1, Ordering::Relaxed);
    // t0 is read before arrival, so arrival_ns is never negative.
    let t0 = *shared.t0.get_or_init(Instant::now);
    let arrival = Instant::now();
    let client_id = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let arrival_ns = nanos(arrival - t0);
    // From here on, the request gets a line however it ends.
    let mut line = LogLine {
        shared: Arc::clone(&shared),
        t0,
        record: Some(Record {
            seq,
            completed: None,
            arrival_ns,
            scripted_service_ns: None,
            stall_wait_ns: None,
            sent_ns: None,
            abandoned_ns: None,
            client_id,
            stream: None,
        }),
    };
    // Read so the connection can be reused; only stream, the token limit and
    // include_usage matter.
    let options = RequestOptions::parse(&axum::body::to_bytes(body, MAX_BODY_BYTES).await.unwrap_or_default());

    let script = &shared.script;
    if options.stream {
        let n = script.stream.output_tokens;
        // min(token limit, N), but at least one token.
        let tokens = options.max_tokens.map_or(n, |max| max.min(n)).max(1);
        let scripted_ttft_ns = script.scripted_ttft_ns(seq);
        let record = line.record();
        record.scripted_service_ns = Some(scripted_ttft_ns);
        record.stall_wait_ns = Some(0);
        record.stream = Some(StreamRecord {
            stream: true,
            endpoint: endpoint.path(),
            scripted_ttft_ns,
            scripted_itl_ns: Vec::new(),
            token_ns: Vec::new(),
            scheduled_token_ns: Vec::new(),
            token_stall_wait_ns: Vec::new(),
            role_chunk_ns: None,
            output_tokens: tokens,
        });
        let request = StreamRequest {
            endpoint,
            seq,
            t0,
            arrival_ns,
            tokens,
            include_usage: options.include_usage,
        };
        return stream_response(line, request);
    }
    let stall_wait_ns = script.stall_wait_ns(arrival_ns);
    let scripted_service_ns = script.scripted_service_ns(seq);
    let record = line.record();
    record.scripted_service_ns = Some(scripted_service_ns);
    record.stall_wait_ns = Some(stall_wait_ns);
    // Held until the window ends, measured from arrival, however long the
    // body took; then serviced.
    sleep_until(arrival + Duration::from_nanos(stall_wait_ns)).await;
    sleep(Duration::from_nanos(scripted_service_ns)).await;

    let response = Json(endpoint.completion(seq)).into_response();
    let sent_ns = nanos(t0.elapsed());
    line.finish(sent_ns);
    response
}

/// One streamed request, as the handler saw it.
struct StreamRequest {
    endpoint: Endpoint,
    seq: u64,
    t0: Instant,
    arrival_ns: u64,
    tokens: u64,
    include_usage: bool,
}

/// Answers with an event stream fed by `emit_tokens`, which runs as its own
/// task. The channel is unbounded, so handing a chunk off never waits: the
/// time taken just before the hand-off is the time it entered the stream.
fn stream_response(line: LogLine, request: StreamRequest) -> Response {
    let (tx, rx) = tokio_mpsc::unbounded_channel::<Event>();
    tokio::spawn(emit_tokens(line, request, tx));
    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok::<_, Infallible>(event), rx))
    });
    Sse::new(events).into_response()
}

/// Emits a stream on the script, then logs it.
///
/// The schedule is absolute: token 0 is scheduled at arrival + TTFT, and
/// token k at token k-1's scheduled time + ITL_k, never at its actual
/// hand-off, so timer overshoot does not accumulate. A token scheduled inside
/// a stall window is moved to the window's end, and every later token moves
/// with it, keeping its scripted gap. The chat role chunk goes out just before
/// token 0 (`RoleChunk::WithFirstToken`), so a client that times the first
/// chunk and one that times the first content chunk see the same TTFT, or on
/// arrival (`RoleChunk::AtArrival`), so they do not.
///
/// If the client goes away (the response stream, and with it the receiver,
/// is dropped), the wait for the next token ends at once and the line is
/// written as abandoned, with the tokens handed off so far.
async fn emit_tokens(mut line: LogLine, request: StreamRequest, tx: tokio_mpsc::UnboundedSender<Event>) {
    let StreamRequest {
        endpoint,
        seq,
        t0,
        arrival_ns,
        tokens,
        include_usage,
    } = request;
    let shared = Arc::clone(&line.shared);
    let script = &shared.script;
    let event = |chunk: Value| Event::default().data(chunk.to_string());
    // False if the client has gone.
    let send_role = |line: &mut LogLine| match endpoint.role_chunk(seq) {
        Some(role) => {
            let role = event(role);
            let handed_off = nanos(t0.elapsed());
            let sent = tx.send(role).is_ok();
            if sent {
                line.stream().role_chunk_ns = Some(handed_off);
            }
            sent
        }
        None => true,
    };
    if script.role_chunk == RoleChunk::AtArrival && !send_role(&mut line) {
        return;
    }
    let mut scheduled = arrival_ns.saturating_add(line.stream().scripted_ttft_ns);
    for k in 0..tokens {
        let itl = (k > 0).then(|| script.scripted_itl_ns(seq, k));
        scheduled = scheduled.saturating_add(itl.unwrap_or(0));
        let wait = script.stall_wait_ns(scheduled);
        scheduled = scheduled.saturating_add(wait);
        tokio::select! {
            () = sleep_until(t0 + Duration::from_nanos(scheduled)) => {}
            () = tx.closed() => return,
        }
        if k == 0 && script.role_chunk == RoleChunk::WithFirstToken && !send_role(&mut line) {
            return;
        }
        let chunk = event(endpoint.token_chunk(seq, &script.token_text, k + 1 == tokens));
        let handed_off = nanos(t0.elapsed());
        if tx.send(chunk).is_err() {
            return;
        }
        let record = line.record();
        record.scripted_service_ns = record.scripted_service_ns.map(|ns| ns.saturating_add(itl.unwrap_or(0)));
        record.stall_wait_ns = record.stall_wait_ns.map(|ns| ns.saturating_add(wait));
        let stream = line.stream();
        stream.scripted_itl_ns.extend(itl);
        stream.token_ns.push(handed_off);
        stream.scheduled_token_ns.push(scheduled);
        stream.token_stall_wait_ns.push(wait);
    }
    if include_usage && tx.send(event(endpoint.usage_chunk(seq, tokens))).is_err() {
        return;
    }
    let done = Event::default().data("[DONE]");
    let sent_ns = nanos(t0.elapsed());
    if tx.send(done).is_err() {
        return;
    }
    line.finish(sent_ns);
}

async fn healthz() -> &'static str {
    "ok"
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn max_tokens(body: Value) -> Option<u64> {
        RequestOptions::parse(body.to_string().as_bytes()).max_tokens
    }

    #[test]
    fn max_tokens_and_max_completion_tokens_are_read() {
        assert_eq!(max_tokens(json!({})), None);
        assert_eq!(max_tokens(json!({"max_tokens": 3})), Some(3));
        assert_eq!(max_tokens(json!({"max_completion_tokens": 3})), Some(3));
        // Both present: max_completion_tokens wins, whether smaller or larger.
        assert_eq!(
            max_tokens(json!({"max_tokens": 4, "max_completion_tokens": 2})),
            Some(2)
        );
        assert_eq!(
            max_tokens(json!({"max_tokens": 2, "max_completion_tokens": 4})),
            Some(4)
        );
        // Anything but a whole number is not a limit, so the other one applies.
        assert_eq!(
            max_tokens(json!({"max_tokens": 2, "max_completion_tokens": "4"})),
            Some(2)
        );
        assert_eq!(
            max_tokens(json!({"max_tokens": 2.5, "max_completion_tokens": 4})),
            Some(4)
        );
        assert_eq!(max_tokens(json!({"max_completion_tokens": -1})), None);
    }
}
