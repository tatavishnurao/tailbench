# tailbench

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

status: pre-release

A latency harness that audits its own numbers.

## The problem

Most load generators are closed loop: they send the next request only after the previous one returns. When the server freezes, the generator waits with it, so the requests that would have been sent during the freeze are never timed. This is coordinated omission, and it makes a freeze nearly invisible in the percentiles.

`tests/stall.rs` runs the same fake server, with a 500 ms freeze, under both runners:

| runner | p50 ms | p99 ms | p99.9 ms | max ms |
|:---|---:|---:|---:|---:|
| closed loop | 1.000 | 1.000 | 1.000 | 501.247 |
| open loop | 1.000 | 401.151 | 491.007 | 501.247 |

Source: `cargo test --test stall -- --nocapture`, on linux-x86_64-24cpu (Intel Core i7-14650HX, kernel 6.6.87.2-microsoft-standard-WSL2), debug build. The test runs on tokio's paused clock, so these values do not depend on the machine. They are HdrHistogram values, which read up to 0.1% high (a true 501 ms reads as 501.247).

The closed loop reports a perfect p99. The open loop shows that at least 1 in 100 requests took 401 ms or more.

## What tailbench does differently

- **Intended-time latency.** The open-loop runner measures each request from the time it was scheduled to be sent, not the time it was actually sent. It also records scheduler lag (actual send minus intended send) and service time for every request, so a slow client is not blamed on the server.
- **A manifest before the first request.** `run` writes and flushes a JSON manifest before it sends anything, and sends nothing if that write fails. A real one, trimmed, from `cargo test --test realtime scheduler_lag_is_recorded_on_the_real_clock -- --nocapture` on linux-x86_64-24cpu:

  ```json
  {
    "tool_version": "0.1.0",
    "build_git_sha": "d712c1e11f603f00f7c1e1bcceb832a47c2298a4",
    "build_git_dirty": true,
    "runtime_git_sha": "d712c1e11f603f00f7c1e1bcceb832a47c2298a4",
    "runtime_git_dirty": true,
    "sha_mismatch": false,
    "kernel": "6.6.87.2-microsoft-standard-WSL2",
    "cpu_model": "Intel(R) Core(TM) i7-14650HX",
    "machine_label": "linux-x86_64-24cpu",
    "rust_version": "rustc 1.94.1 (e408947bf 2026-03-25)",
    "build_profile": "debug",
    "opt_level": "0",
    "runner": "open_loop",
    "interval_ns": 5000000,
    "count": 1000,
    "paused_clock": false,
    "runtime_flavor": "current_thread",
    "runtime_workers": 1
  }
  ```

  `dirty` is true because this README was written with uncommitted changes in the tree.
- **Provenance.** The manifest records the git SHA the binary was built from (`build_git_sha`) and the SHA of the checkout at run time (`runtime_git_sha`). It records whether each was dirty, and sets `sha_mismatch` when the two differ. Anything that cannot be determined is written as `"UNKNOWN"`, never guessed.
- **Verdicts that can refuse.** `compare` runs A and B several times each and names a direction only when the runs support one. Otherwise it says "inconclusive" and gives the reason, or refuses outright. Three real lines from `cargo test --test compare -- --nocapture --test-threads=1` (paused clock, linux-x86_64-24cpu):

  ```text
  B slower at p99 (mean per-run) by 891.0 ms [888.5, 893.5], 5 vs 5 runs, Welch t df 4.6, nominal 95%
  inconclusive at p99 (mean per-run): episode-limited: 81 tail episodes at or above A's pooled p99 (14.0 ms) and 1 at or above B's (516.0 ms), want >= 5 each; CI of delta crosses 0 (delta +178.0 ms [-315.9, +671.9]), 5 vs 5 runs, Welch t df 4.0, nominal 95%
  refused at p99: runners differ: open-loop and closed-loop latencies measure different things; closed-loop runs (A: 0, B: 5) measure latency from the actual send, so coordinated omission hides stalls; rerun open-loop, 5 vs 5 runs
  ```

  The first line comes from a stall in every B run. The second comes from a stall in 1 of 5 B runs: a large delta, but from one event, so it gives no direction. The third compares open-loop runs with closed-loop runs, which `compare` will not do.

## How trustworthy are the verdicts

`compare` was calibrated on synthetic runs. The settings are:

- between-run sd of 0 to 20%;
- AR(1) correlation of 0.5 and 0.9;
- with and without stalls;
- R = 3, 5 and 10 runs per side.

A false verdict means naming a direction when A and B come from the same distribution. The target is at most 0.05. Results for the default method (per-run Welch), from the Summary in [docs/calibration.md](docs/calibration.md) (linux-x86_64-24cpu, 300 trials per cell, Monte Carlo SE 0.013):

| method | R | worst false-verdict rate | where | cells significantly above 0.05 |
|:---|---:|---:|:---|---:|
| Welch | 3 | 0.053 | phi 0.9, sd 10%, stalls, p50 | 0 of 32 |
| Welch | 5 | 0.057 | phi 0.9, sd 20%, stalls, p50 | 0 of 32 |
| Welch | 10 | 0.067 | phi 0.9, sd 0%, stalls, p50 | 0 of 32 |

- **Power.** With R = 10 and no stalls, Welch detects a +10% p99 regression in 62–88% of trials at correlation 0.5, but only in 21–28% at correlation 0.9 (docs/calibration.md, power table).
- **Stalls.** When a stall covers more than 1% of a run, that run's p99 lands inside the stall. Per-run p99 then cannot detect a regression in the rest of the distribution: Welch at R = 10 detects a +20% regression in at most 4% of trials with stalls (docs/calibration.md). A verdict that splits the body from the stall episodes is planned.

## Quick start

tailbench is a library for now: there is no CLI and nothing is published to crates.io. Clone the repository and run the tests that produce the numbers above:

```sh
cargo test                                                   # everything except the slow, ignored tests
cargo test --test stall -- --nocapture                       # section 2: closed vs open loop
cargo test --test realtime -- --nocapture                    # a real manifest and scheduler lag on your machine
cargo test --test compare -- --nocapture --test-threads=1    # verdict lines
TAILBENCH_WRITE_CALIBRATION=1 cargo test --release --lib calibration_report -- --ignored --nocapture
                                                             # regenerates docs/calibration.md (331 s on linux-x86_64-24cpu)
```

## Limits

- **Send precision is about 1 ms.** Sends wait on tokio's timer, which tokio 1.53.3 documents as follows: "`Sleep` operates at millisecond granularity and should not be used for tasks that require high-resolution timers." On linux-x86_64-24cpu under WSL2, scheduler lag was p50 1.192 ms, p99 1.761 ms, max 1.857 ms (`cargo test --test realtime scheduler_lag_is_recorded_on_the_real_clock -- --nocapture`, debug build, current_thread runtime). That is why `compare::DEFAULT_MIN_EFFECT`, the smallest difference a verdict should name, is 1 ms.
- **Skipped tests show as passed.** The git provenance tests return early when git is missing, or when the crate is not the top level of its own repository (vendored, or from a tarball). They print `skipping: ...` to stderr, but cargo still counts them as passed.
- **Runtime git fields read the build-time source directory.** `runtime_git_sha` and `runtime_git_dirty` probe the directory the crate was compiled from, at run time. A binary copied to another machine, or run after that checkout has moved, reports `UNKNOWN` or describes whatever is now at that path.
- **Calibration is synthetic only.** The false-verdict and power numbers come from a model: lognormal body, AR(1) correlation, per-run offsets, flat 300 ms stalls. They have not been checked against real services.
- **Library only.** There is no CLI and no HTTP target yet. You supply the request as an async closure.

## Roadmap

- A verdict that splits the body from stall episodes, so a stall rate and a body regression are reported separately.
- phantom: a ground-truth OpenAI-compatible server with scripted TTFT/ITL and canary stalls, to test benchmarks against known answers.
- Audit adapters that check the output of existing LLM benchmark tools.
- A CLI.

## License

Apache-2.0. See [LICENSE](LICENSE).
