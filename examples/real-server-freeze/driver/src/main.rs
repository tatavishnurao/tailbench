//! `rs-drive`: one POST /v1/completions per scheduled slot, timed by tailbench.
//!
//! The summary goes to stdout as one JSON object; PREFIX.csv holds
//! `intended_offset_ns,latency_ns` per request, in intended-send order.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tailbench::{RunConfig, RunnerKind, Sample, run};

struct Args {
    mode: RunnerKind,
    url: String,
    model: String,
    rate: f64,
    duration: f64,
    out: String,
}

fn parse() -> Result<Args, String> {
    let mut mode = None;
    let (mut url, mut model, mut out) = (None, None, None);
    let (mut rate, mut duration) = (10.0, 25.0);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--mode" => {
                mode = Some(match value()?.as_str() {
                    "open" => RunnerKind::OpenLoop,
                    "closed" => RunnerKind::ClosedLoop,
                    other => return Err(format!("--mode must be open or closed, got {other}")),
                })
            }
            "--url" => url = Some(value()?),
            "--model" => model = Some(value()?),
            "--out" => out = Some(value()?),
            "--rate" => rate = value()?.parse().map_err(|e| format!("--rate: {e}"))?,
            "--duration" => duration = value()?.parse().map_err(|e| format!("--duration: {e}"))?,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if !(rate > 0.0 && duration > 0.0) {
        return Err("--rate and --duration must be positive".into());
    }
    Ok(Args {
        mode: mode.ok_or("--mode is required")?,
        url: url.ok_or("--url is required")?,
        model: model.ok_or("--model is required")?,
        rate,
        duration,
        out: out.ok_or("--out is required")?,
    })
}

/// Nearest rank: the ceil(q * n)-th smallest value, in milliseconds.
fn nearest_rank_ms(sorted_ns: &[u64], q: f64) -> f64 {
    let rank = ((q * sorted_ns.len() as f64).ceil() as usize).clamp(1, sorted_ns.len());
    sorted_ns[rank - 1] as f64 / 1e6
}

fn unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rs-drive: {e}");
            return ExitCode::from(2);
        }
    };
    let body = serde_json::json!({
        "model": args.model, "prompt": "Hello", "max_tokens": 16, "temperature": 0
    });
    let client = reqwest::Client::new();
    let url = format!("{}/v1/completions", args.url.trim_end_matches('/'));
    let errors = Arc::new(AtomicU64::new(0));
    // Wall clock at the first send; send 0 is due at the schedule origin, so
    // this is the origin plus send 0's scheduler lag.
    let origin = Arc::new(AtomicU64::new(0));
    let config = RunConfig {
        kind: args.mode,
        interval: Duration::from_secs_f64(1.0 / args.rate),
        count: (args.rate * args.duration).round() as u64,
        seed: None,
        extra: Default::default(),
    };
    let call = {
        let (errors, origin) = (Arc::clone(&errors), Arc::clone(&origin));
        move || {
            let _ = origin.compare_exchange(0, unix_ns(), Ordering::Relaxed, Ordering::Relaxed);
            let (client, url, body, errors) = (client.clone(), url.clone(), body.clone(), Arc::clone(&errors));
            async move {
                let ok = match client.post(&url).json(&body).send().await {
                    Ok(resp) => resp.status().is_success() && resp.bytes().await.is_ok(),
                    Err(_) => false,
                };
                if !ok {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    };
    // The run manifest goes to stderr so stdout stays one JSON object.
    let result = match run(&config, std::io::stderr(), call).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rs-drive: {e}");
            return ExitCode::FAILURE;
        }
    };
    match write_outputs(
        &args,
        &result.samples,
        errors.load(Ordering::Relaxed),
        origin.load(Ordering::Relaxed),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rs-drive: {e}");
            ExitCode::FAILURE
        }
    }
}

fn write_outputs(args: &Args, samples: &[Sample], errors: u64, origin_unix_ns: u64) -> std::io::Result<()> {
    let mut csv = BufWriter::new(File::create(format!("{}.csv", args.out))?);
    writeln!(csv, "intended_offset_ns,latency_ns")?;
    for s in samples {
        writeln!(csv, "{},{}", s.intended_offset_ns, s.latency_ns)?;
    }
    csv.flush()?;
    let mut lat: Vec<u64> = samples.iter().map(|s| s.latency_ns).collect();
    lat.sort_unstable();
    println!(
        "{}",
        serde_json::json!({
            "n": lat.len(),
            "errors": errors,
            "p50": nearest_rank_ms(&lat, 0.50),
            "p90": nearest_rank_ms(&lat, 0.90),
            "p99": nearest_rank_ms(&lat, 0.99),
            "p99.9": nearest_rank_ms(&lat, 0.999),
            "max": lat.last().copied().unwrap_or(0) as f64 / 1e6,
            "origin_unix_ns": origin_unix_ns,
        })
    );
    Ok(())
}
