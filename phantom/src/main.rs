//! phantom CLI: validates the script before binding or creating the log, then
//! serves until SIGINT or SIGTERM and shuts down gracefully, so every answered
//! request is in the log.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{ArgGroup, CommandFactory, Parser};
use phantom::{
    DEFAULT_ITL, DEFAULT_OUTPUT_TOKENS, RoleChunk, Script, ServiceSpec, Stall, StreamSpec, parse_itl_lognormal,
    parse_itl_ms, parse_lognormal, parse_output_tokens, parse_role_chunk, parse_service_ms, parse_stall,
    parse_ttft_lognormal, parse_ttft_ms,
};
use tokio::net::TcpListener;

/// Deterministic fake OpenAI-compatible server that logs ground truth for every request.
#[derive(Parser)]
#[command(version)]
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
    /// Tokens per streamed response; a request's smaller max_tokens wins.
    #[arg(long, value_name = "N", value_parser = parse_output_tokens, default_value_t = DEFAULT_OUTPUT_TOKENS,
          allow_hyphen_values = true)]
    output_tokens: u64,
    /// When a chat stream's role chunk is sent: with-first-token or at-arrival.
    #[arg(long, value_name = "WHEN", value_parser = parse_role_chunk, default_value = "with-first-token")]
    role_chunk: RoleChunk,
    /// Freeze from START_MS to START_MS + LEN_MS after the first request's arrival. Repeatable.
    #[arg(long, value_name = "START_MS:LEN_MS", value_parser = parse_stall, allow_hyphen_values = true)]
    stall: Vec<Stall>,
    /// Ground-truth JSONL log (created, or truncated).
    #[arg(long, value_name = "PATH")]
    log: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
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
        .unwrap_or_else(|err| Cli::command().error(ErrorKind::ValueValidation, err).exit());
    match run(cli.listen, script, cli.log).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("phantom: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(listen: SocketAddr, script: Script, log: PathBuf) -> io::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    let phantom = phantom::start(listener, script, &log).await?;
    eprintln!(
        "phantom {} listening on {}, logging to {}",
        phantom::VERSION,
        phantom.local_addr(),
        log.display()
    );
    let signalled = shutdown_signal().await;
    eprintln!("phantom: shutting down");
    // Shut down even if waiting for a signal failed, so the log is flushed.
    phantom.shutdown().await.and(signalled)
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
