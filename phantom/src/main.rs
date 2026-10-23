//! phantom CLI: validates the script before binding or creating the log, then
//! serves until SIGINT or SIGTERM and shuts down gracefully, so every answered
//! request is in the log. With `--live` on a terminal it redraws a panel of
//! what it sees on stderr.
//!
//! `phantom report` reads a log back instead, and `phantom demo` shows
//! coordinated omission in one command; both are dispatched before the
//! server's arguments are parsed, so they are unchanged.

mod demo;

use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{ArgGroup, CommandFactory, Parser};
use phantom::live::{Monitor, Numbers, PanelHeader, render_panel, render_summary, use_color};
use phantom::report::{Report, parse_rate};
use phantom::{
    DEFAULT_ITL, DEFAULT_OUTPUT_TOKENS, DEFAULT_TOKEN_TEXT, RoleChunk, Script, ServiceSpec, Stall, StreamSpec,
    parse_itl_lognormal, parse_itl_ms, parse_lognormal, parse_output_tokens, parse_role_chunk, parse_service_ms,
    parse_stall, parse_token_text, parse_ttft_lognormal, parse_ttft_ms,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How often the live panel is redrawn.
const REDRAW_EVERY: Duration = Duration::from_millis(100);
/// How many redraws between terminal width checks.
const WIDTH_EVERY: u32 = 10;

/// Deterministic fake OpenAI-compatible server that logs ground truth for every request.
#[derive(Parser)]
// The package is tailbench-phantom; keep the command's name the binary's.
#[command(name = "phantom", version)]
#[command(
    after_help = "To read a log back: phantom report --rate R LOG.jsonl [--html OUT.html]\nTo see coordinated omission in 15 s: phantom demo"
)]
#[command(group(ArgGroup::new("service").required(true).args(["service_ms", "lognormal"])))]
struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Seed for lognormal service times.
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    seed: u64,
    /// Fixed service time per request, in whole milliseconds.
    #[arg(long, value_name = "MS", value_parser = parse_service_ms, allow_hyphen_values = true)]
    service_ms: Option<ServiceSpec>,
    /// Lognormal service time: median in milliseconds, and sigma of the underlying normal.
    #[arg(long, value_name = "MEDIAN_MS:SIGMA", value_parser = parse_lognormal, allow_hyphen_values = true)]
    lognormal: Option<ServiceSpec>,
    /// Streaming time to first token, in whole milliseconds. Default: the service time spec.
    #[arg(long, value_name = "MS", value_parser = parse_ttft_ms, allow_hyphen_values = true)]
    ttft_ms: Option<ServiceSpec>,
    /// Lognormal streaming time to first token: median in milliseconds, and sigma.
    #[arg(long, value_name = "MEDIAN_MS:SIGMA", value_parser = parse_ttft_lognormal, allow_hyphen_values = true,
          conflicts_with = "ttft_ms")]
    ttft_lognormal: Option<ServiceSpec>,
    /// Streaming inter-token latency, in whole milliseconds. Default: 10.
    #[arg(long, value_name = "MS", value_parser = parse_itl_ms, allow_hyphen_values = true)]
    itl_ms: Option<ServiceSpec>,
    /// Lognormal streaming inter-token latency: median in milliseconds, and sigma.
    #[arg(long, value_name = "MEDIAN_MS:SIGMA", value_parser = parse_itl_lognormal, allow_hyphen_values = true,
          conflicts_with = "itl_ms")]
    itl_lognormal: Option<ServiceSpec>,
    /// Tokens per streamed response; a request's smaller max_completion_tokens (else max_tokens) wins.
    #[arg(long, value_name = "N", value_parser = parse_output_tokens, default_value_t = DEFAULT_OUTPUT_TOKENS,
          allow_hyphen_values = true)]
    output_tokens: u64,
    /// When a chat stream's role chunk is sent: with-first-token or at-arrival.
    #[arg(long, value_name = "WHEN", value_parser = parse_role_chunk, default_value = "with-first-token")]
    role_chunk: RoleChunk,
    /// The text every streamed token carries: one chunk is one token.
    #[arg(long, value_name = "TEXT", value_parser = parse_token_text, default_value = DEFAULT_TOKEN_TEXT,
          allow_hyphen_values = true)]
    token_text: String,
    /// Freeze from START_MS to START_MS + LEN_MS after the first request's arrival. Repeatable.
    #[arg(long, value_name = "START_MS:LEN_MS", value_parser = parse_stall, allow_hyphen_values = true)]
    stall: Vec<Stall>,
    /// Ground-truth JSONL log (created, or truncated).
    #[arg(long, value_name = "PATH")]
    log: PathBuf,
    /// Redraw a panel on stderr ~10 times a second: arrivals, stalls, and the p99 your load
    /// tool reports against the p99 its users waited (only when stderr is a terminal).
    #[arg(long)]
    live: bool,
}

/// Open-loop truth and server time for every request in a phantom log.
#[derive(Parser)]
#[command(name = "phantom report", version)]
struct ReportCli {
    /// The rate the load generator was asked for, in requests per second: request seq i was
    /// meant to be sent i / RATE seconds after the schedule start.
    #[arg(long, value_name = "R", value_parser = parse_rate)]
    rate: i64,
    /// Also write a self-contained HTML page (inline SVG timeline, percentiles, script).
    #[arg(long, value_name = "OUT.html")]
    html: Option<PathBuf>,
    /// phantom's ground-truth JSONL log.
    #[arg(value_name = "LOG.jsonl")]
    log: PathBuf,
}

/// Coordinated omission in one command: a naive client and tailbench's open loop against the
/// same frozen server, each set against the truth from phantom's log. Takes about 15 s.
#[derive(Parser)]
#[command(name = "phantom demo", version)]
struct DemoCli {}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "report") {
        args.remove(1);
        return report(ReportCli::parse_from(args));
    }
    if args.get(1).is_some_and(|a| a == "demo") {
        args.remove(1);
        DemoCli::parse_from(args);
        return demo::demo();
    }
    serve(Cli::parse_from(args))
}

fn report(cli: ReportCli) -> ExitCode {
    let fail = |msg: String| {
        eprintln!("phantom report: {msg}");
        ExitCode::FAILURE
    };
    let text = match std::fs::read_to_string(&cli.log) {
        Ok(text) => text,
        Err(err) => return fail(format!("cannot read {}: {err}", cli.log.display())),
    };
    let report = match Report::from_log(&text, cli.rate) {
        Ok(report) => report,
        Err(err) => return fail(format!("{}: {err}", cli.log.display())),
    };
    let name = cli.log.display().to_string();
    print!("{}", report.text(&name, terminal_width()));
    if let Some(html) = cli.html {
        if let Err(err) = std::fs::write(&html, report.html(&name)) {
            return fail(format!("cannot write {}: {err}", html.display()));
        }
        println!("wrote {}", html.display());
    }
    ExitCode::SUCCESS
}

#[tokio::main]
async fn serve(cli: Cli) -> ExitCode {
    let service = cli
        .service_ms
        .or(cli.lognormal)
        .expect("clap requires one service spec");
    let stream = StreamSpec {
        ttft: cli.ttft_ms.or(cli.ttft_lognormal),
        itl: cli.itl_ms.or(cli.itl_lognormal).unwrap_or(DEFAULT_ITL),
        output_tokens: cli.output_tokens,
    };
    let script = Script::new(cli.seed, service, cli.stall)
        .and_then(|script| script.with_stream(stream))
        .map(|script| script.with_role_chunk(cli.role_chunk))
        .and_then(|script| script.with_token_text(&cli.token_text))
        .unwrap_or_else(|err| Cli::command().error(ErrorKind::ValueValidation, err).exit());
    match run(cli.listen, script, cli.log, cli.live).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("phantom: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(listen: SocketAddr, script: Script, log: PathBuf, live: bool) -> io::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    let phantom = phantom::start(listener, script, &log).await?;
    eprintln!(
        "phantom {} listening on {}, logging to {}",
        phantom::VERSION,
        phantom.local_addr(),
        log.display()
    );
    // Off the terminal, --live changes nothing.
    let panel = (live && io::stderr().is_terminal()).then(|| {
        let header = PanelHeader {
            listen: phantom.local_addr().to_string(),
            title: None,
        };
        Panel::spawn(phantom.monitor(), header)
    });
    let signalled = shutdown_signal().await;
    let Some(panel) = panel else {
        eprintln!("phantom: shutting down");
        // Shut down even if waiting for a signal failed, so the log is flushed.
        return phantom.shutdown().await.and(signalled);
    };
    // The panel keeps drawing while in-flight requests drain, then leaves
    // its final frame and a summary.
    let shut = phantom.shutdown().await;
    panel.finish(None).await;
    shut.and(signalled)
}

/// The `--live` panel's redraw task (also `phantom demo`'s). Spawn it only
/// when stderr is a terminal.
struct Panel {
    stop: oneshot::Sender<Option<Numbers>>,
    task: JoinHandle<()>,
}

impl Panel {
    fn spawn(monitor: Monitor, header: PanelHeader) -> Self {
        let (stop, mut stopped) = oneshot::channel();
        let color = use_color(std::env::var_os("NO_COLOR").as_deref(), io::stderr().is_terminal());
        let task = tokio::spawn(async move {
            let mut drawn = 0;
            let mut width = terminal_width();
            let mut ticks = tokio::time::interval(REDRAW_EVERY);
            let mut last = None;
            for frame in 1u32.. {
                tokio::select! {
                    _ = ticks.tick() => {}
                    numbers = &mut stopped => {
                        last = numbers.ok().flatten();
                        break;
                    }
                }
                if frame % WIDTH_EVERY == 0 {
                    width = terminal_width();
                }
                let snap = monitor.snapshot();
                drawn = draw(&render_panel(&header, &snap, &snap.numbers(), width, color), drawn);
            }
            let snap = monitor.snapshot();
            let numbers = last.unwrap_or_else(|| snap.numbers());
            let text = render_panel(&header, &snap, &numbers, width, color) + &render_summary(&snap, width, color);
            draw(&text, drawn);
        });
        Self { stop, task }
    }

    /// Stops redrawing and leaves the final frame, showing `numbers` in
    /// place of the estimate when given.
    async fn finish(self, numbers: Option<Numbers>) {
        let _ = self.stop.send(numbers);
        let _ = self.task.await;
    }
}

/// Replaces the `drawn` lines last drawn (and anything typed after them,
/// such as ^C) with `text`, in one write; returns its line count.
fn draw(text: &str, drawn: usize) -> usize {
    let mut frame = String::new();
    if drawn > 0 {
        // To column 0 of the panel's first line, then clear to the end of
        // the screen.
        frame.push_str(&format!("\x1b[{drawn}F"));
    } else {
        frame.push('\r');
    }
    frame.push_str("\x1b[J");
    frame.push_str(text);
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(frame.as_bytes()).and_then(|()| stderr.flush());
    text.matches('\n').count()
}

/// The terminal's width in columns: from `stty size` on the controlling
/// terminal, else `$COLUMNS`, else 80.
fn terminal_width() -> usize {
    stty_columns()
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .filter(|&w| w > 0)
        .unwrap_or(80)
}

#[cfg(unix)]
fn stty_columns() -> Option<usize> {
    let tty = std::fs::File::open("/dev/tty").ok()?;
    let out = std::process::Command::new("stty")
        .arg("size")
        .stdin(tty)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    text.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(unix))]
fn stty_columns() -> Option<usize> {
    None
}

#[cfg(unix)]
async fn shutdown_signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        res = tokio::signal::ctrl_c() => res,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}
