//! `phantom demo`: coordinated omission in one command, in-process.
//!
//! Two runs against the same script (1 ms service, one freeze), each on a
//! fresh phantom on an ephemeral loopback port: first tailbench's closed-loop
//! runner, a naive client that sends one request at a time, waits for each
//! reply and measures from its actual send; then tailbench's open-loop
//! runner, which sends on schedule and measures from the intended send. Both
//! are asked for 100 requests/s. Each tool's own p99 is then set against the
//! open-loop truth computed from its run's log, as `phantom report` does; the
//! final panel of each run and the summary show the same two numbers.
//!
//! The client is a minimal HTTP/1.1 POST over tokio's `TcpStream`, with a
//! pool of keep-alive connections: enough for phantom's fixed-length JSON
//! responses, and no HTTP client dependency.

use std::io::{self, IsTerminal};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phantom::live::{Numbers, PanelHeader, fmt_p99, fmt_ratio, render_panel, render_summary, use_color};
use phantom::report::Report;
use phantom::{Script, ServiceSpec, Stall};
use tailbench::RunResult;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Builder, Runtime};

use crate::{Panel, terminal_width};

/// Set (to anything but the empty string) to run 2 s per run with a 500:300
/// freeze instead of 6 s with 2000:500; for the integration test.
pub const QUICK_ENV: &str = "PHANTOM_DEMO_QUICK";

/// Requests per second both runs are asked for.
const RATE: u64 = 100;
/// A tool is marked ✓ when its p99 is within this fraction of the truth.
const TOLERANCE: f64 = 0.10;

const CHAT: &[u8] = b"POST /v1/chat/completions HTTP/1.1\r\nhost: 127.0.0.1\r\n\
content-type: application/json\r\ncontent-length: 2\r\n\r\n{}";
const HEALTH: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n";

/// How long each run lasts and where its freeze is.
struct Plan {
    secs: u64,
    stall: Stall,
}

impl Plan {
    fn from_env() -> Self {
        let quick = std::env::var_os(QUICK_ENV).is_some_and(|v| !v.is_empty());
        if quick {
            Self {
                secs: 2,
                stall: Stall {
                    start_ms: 500,
                    len_ms: 300,
                },
            }
        } else {
            Self {
                secs: 6,
                stall: Stall {
                    start_ms: 2_000,
                    len_ms: 500,
                },
            }
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / RATE)
    }
}

#[derive(Clone, Copy)]
enum Client {
    Naive,
    OpenLoop,
}

/// One run's numbers.
struct Outcome {
    title: String,
    /// The p99 the load tool measured itself, in ns.
    tool_p99_ns: i64,
    /// Open-loop truth p99 from the run's log, in ns.
    truth_p99_ns: i64,
    log: PathBuf,
}

impl Outcome {
    /// What the run's final panel shows: the same two numbers as the
    /// summary.
    fn numbers(&self) -> Numbers {
        Numbers {
            tool_p99_ns: Some(self.tool_p99_ns),
            measured: true,
            truth_p99_ns: Some(self.truth_p99_ns),
            rate_per_s: Some(RATE as f64),
        }
    }

    /// `truth / tool`, as the panel's verdict computes it.
    fn ratio(&self) -> Option<f64> {
        self.numbers().ratio()
    }
}

pub fn demo() -> ExitCode {
    match run_demo() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("phantom demo: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_demo() -> io::Result<ExitCode> {
    let plan = Plan::from_env();
    let dir = std::env::temp_dir().join(format!("phantom-demo-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    // Separate runtimes, so phantom's timers and the client's schedule do not
    // share worker threads.
    let runtime = |name: &str| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name(name)
            .enable_all()
            .build()
    };
    let (server_rt, client_rt) = (runtime("phantom")?, runtime("client")?);
    let failed = Arc::new(AtomicU64::new(0));
    let run = |title: &str, client, file: &str| {
        run_one(&plan, title, client, &dir.join(file), &server_rt, &client_rt, &failed)
    };
    let naive = run("1/2 naive client", Client::Naive, "naive.jsonl")?;
    let open = run("2/2 tailbench open loop", Client::OpenLoop, "tailbench.jsonl")?;
    let logs = [naive.log.clone(), open.log.clone()];
    let failed = failed.load(Ordering::Relaxed);

    let color = use_color(std::env::var_os("NO_COLOR").as_deref(), io::stdout().is_terminal());
    print!("{}", summary(&plan, &[naive, open], color));
    println!();
    println!("logs: {}", logs[0].display());
    println!("      {}", logs[1].display());
    println!(
        "next: phantom report --rate {RATE} {} --html out.html",
        logs[0].display()
    );
    if failed > 0 {
        eprintln!("phantom demo: {failed} requests failed; the numbers above leave them out");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Starts a fresh phantom, drives it with `client` for the plan's length,
/// shuts it down and reads the truth back from its log.
fn run_one(
    plan: &Plan,
    title: &str,
    client: Client,
    log: &Path,
    server_rt: &Runtime,
    client_rt: &Runtime,
    failed: &Arc<AtomicU64>,
) -> io::Result<Outcome> {
    let script = Script::new(1, ServiceSpec::Fixed { ms: 1 }, vec![plan.stall]).map_err(io::Error::other)?;
    let phantom = server_rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        phantom::start(listener, script, log).await
    })?;
    let addr = phantom.local_addr();
    let header = PanelHeader {
        listen: addr.to_string(),
        title: Some(title.to_owned()),
    };
    let monitor = phantom.monitor();
    let panel = if io::stderr().is_terminal() {
        let _runtime = server_rt.enter();
        Some(Panel::spawn(monitor.clone(), header.clone()))
    } else {
        eprintln!("phantom demo: {title}, {} s", plan.secs);
        None
    };

    let pool = Pool::default();
    let (interval, count) = (plan.interval(), RATE * plan.secs);
    let result: io::Result<RunResult> = client_rt.block_on(async {
        // Opens the first connection before the schedule starts; health
        // checks are not logged and do not set phantom's t0.
        exchange(&pool, addr, HEALTH).await?;
        let call = || post(pool.clone(), addr, Arc::clone(failed));
        Ok(match client {
            Client::Naive => tailbench::closed_loop(interval, count, call).await,
            Client::OpenLoop => tailbench::open_loop(interval, count, call).await,
        })
    });
    let shut = server_rt.block_on(phantom.shutdown());
    let outcome = (|| {
        let result = result?;
        shut?;
        let text = std::fs::read_to_string(log)?;
        let report = Report::from_log(&text, interval.as_nanos() as i64).map_err(io::Error::other)?;
        Ok::<_, io::Error>(Outcome {
            title: title.to_owned(),
            // The histogram is in microseconds.
            tool_p99_ns: (result.latency.hist.value_at_quantile(0.99) * 1_000) as i64,
            truth_p99_ns: report.truth().p99(),
            log: log.to_owned(),
        })
    })();
    // The final frame shows the summary's numbers, not the estimate.
    let numbers = outcome.as_ref().ok().map(Outcome::numbers);
    match panel {
        Some(panel) => server_rt.block_on(panel.finish(numbers)),
        None => {
            if let Some(numbers) = numbers {
                let (snap, width) = (monitor.snapshot(), terminal_width());
                eprint!(
                    "{}{}",
                    render_panel(&header, &snap, &numbers, width, false),
                    render_summary(&snap, width, false)
                );
            }
        }
    }
    outcome
}

/// The summary under both panels: one row per run with the tool's own p99,
/// the truth from its log, and whether they agree (within [`TOLERANCE`]).
fn summary(plan: &Plan, runs: &[Outcome], color: bool) -> String {
    let paint = |text: &str, code: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };
    let freeze = plan.stall;
    let mut out = format!(
        "\n{RATE} req/s for {} s against 1 ms service, server frozen {:.1}–{:.1} s:\n\n",
        plan.secs,
        freeze.start_ms as f64 / 1e3,
        (freeze.start_ms + freeze.len_ms) as f64 / 1e3
    );
    out += &format!("  {:23}  {:>17}  {:>12}\n", "p99", "your tool reports", "users waited");
    for run in runs {
        let (tool, truth) = (run.tool_p99_ns as f64, run.truth_p99_ns as f64);
        let mark = if (tool - truth).abs() <= TOLERANCE * truth {
            paint("✓ matches the truth", "1;32")
        } else if tool < truth {
            let by = run
                .ratio()
                .map_or_else(String::new, |r| format!(" by {}×", fmt_ratio(r)));
            paint(&format!("✗ under{by}"), "1;31")
        } else {
            paint("✗ over", "1;31")
        };
        out += &format!(
            "  {:23}  {:>17}  {:>12}  {mark}\n",
            run.title,
            fmt_p99(Some(run.tool_p99_ns)),
            fmt_p99(Some(run.truth_p99_ns))
        );
    }
    out
}

/// Idle keep-alive connections.
type Pool = Arc<Mutex<Vec<TcpStream>>>;

/// One chat request; a failure is counted, not raised, since the runners'
/// requests return nothing.
async fn post(pool: Pool, addr: SocketAddr, failed: Arc<AtomicU64>) {
    if exchange(&pool, addr, CHAT).await.is_err() {
        failed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Sends `request` on an idle connection (or a new one) and reads the whole
/// response; the connection goes back to the pool only if that worked.
async fn exchange(pool: &Pool, addr: SocketAddr, request: &[u8]) -> io::Result<()> {
    let idle = pool.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let stream = match idle {
        Some(stream) => stream,
        None => {
            let stream = TcpStream::connect(addr).await?;
            stream.set_nodelay(true)?;
            stream
        }
    };
    write_all(&stream, request).await?;
    read_response(&stream).await?;
    pool.lock().unwrap_or_else(|e| e.into_inner()).push(stream);
    Ok(())
}

/// Writes all of `buf`. tokio's readiness API, so no `io-util` feature is
/// needed.
async fn write_all(stream: &TcpStream, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        stream.writable().await?;
        match stream.try_write(buf) {
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reads what is there, waiting until something is; 0 at end of stream.
async fn read(stream: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        stream.readable().await?;
        match stream.try_read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
}

/// Reads one `200` response with a `content-length` body.
async fn read_response(stream: &TcpStream) -> io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = read(stream, &mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(io::Error::other)?;
    if !head.starts_with("HTTP/1.1 200") {
        let status = head.lines().next().unwrap_or_default();
        return Err(io::Error::other(format!("unexpected response: {status}")));
    }
    let len = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .ok_or_else(|| io::Error::other("response without content-length"))?;
    while buf.len() < head_end + len {
        let n = read(stream, &mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use phantom::live::{Snapshot, StallCounts};

    use super::*;

    /// The `N` in the first `by N×` of `text`.
    fn by(text: &str) -> &str {
        let start = text.find(" by ").unwrap_or_else(|| panic!("no ratio in\n{text}")) + 4;
        let len = text[start..].find('×').unwrap();
        &text[start..start + len]
    }

    #[test]
    fn panel_verdict_and_summary_agree() {
        let naive = Outcome {
            title: "1/2 naive client".to_owned(),
            tool_p99_ns: 4_740_000,
            truth_p99_ns: 469_000_000,
            log: PathBuf::from("naive.jsonl"),
        };
        // A closed-loop run seen at its end: nothing arrived in the freeze.
        let snap = Snapshot {
            now_ns: Some(6_000_000_000),
            arrivals: vec![10; 61],
            completed: Vec::new(),
            abandoned: 0,
            stalls: vec![StallCounts {
                start_ns: 2_000_000_000,
                end_ns: 2_500_000_000,
                before: 50,
                during: 0,
            }],
        };
        let header = PanelHeader {
            listen: "127.0.0.1:1".to_owned(),
            title: Some(naive.title.clone()),
        };
        let panel = render_panel(&header, &snap, &naive.numbers(), 100, false);
        // The tool's bar is under one column on the shared scale.
        let row = panel.lines().find(|l| l.contains("your tool reports")).unwrap();
        assert!(row.starts_with("  your tool reports  ▌ "), "{panel}");
        assert!(row.ends_with(" 4.74 ms"), "{panel}");
        let row = panel.lines().find(|l| l.contains("users waited")).unwrap();
        assert!(row.ends_with(" 469 ms"), "{panel}");
        let plan = Plan::from_env();
        let text = summary(&plan, &[naive], false);
        assert!(text.contains("4.74 ms") && text.contains("469 ms"), "{text}");
        assert_eq!(by(&panel), "99", "{panel}");
        assert_eq!(by(&panel), by(&text), "{panel}\n{text}");
    }
}
