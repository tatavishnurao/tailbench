# Changelog

All notable changes to the crates in this repository are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and both crates adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## tailbench

### [0.1.0] - 2026-10-22

#### Added

- Closed-loop and open-loop runners (`closed_loop`, `open_loop`) that record latency into HdrHistogram. The open-loop runner measures latency from each request's intended send time and also records service time and scheduler lag; every run keeps its per-request samples in intended-send order.
- `run` and the provenance manifest: written as JSON before the first request, with the crate version, build-time and run-time git SHA and dirty state, the compiling rustc, the build profile and opt-level, OS, architecture, kernel, CPU model, machine label, the runner's parameters, the tokio runtime flavor and caller labels.
- Statistics (`stats`): moving-block and hierarchical bootstrap confidence intervals for percentiles, a Welch interval, sample-size and tail-episode guards that attach warnings to a CI, and exact binomial utilities.
- Compare verdicts (`compare`): Slower, Faster, Inconclusive or Refused for a percentile between two groups of runs, by per-run Welch (the default) or hierarchical bootstrap, with warnings when manifests, offered load or git state differ between runs.
- Split verdicts (`compare_split`, `episodes`): the body of each run, with stall episodes taken out, compared by percentile, and the episode rate compared by an exact conditional test, with refusal rules and the smallest detectable rate ratio.
- Calibration: a synthetic latency generator and a calibration report (`docs/calibration.md` in the repository) of false-verdict rates across noise, correlation, stalls and split analysis.

## tailbench-phantom

### [0.2.1] - 2026-10-23

#### Fixed

- Open-loop truth counted the idle time between runs as lateness when a phantom served more than one run, or sat idle: two 3 s `oha` runs at 100 req/s with a 6 s gap on one phantom gave a truth p99 of about 6 s against a server p99 of about 4 ms, in `phantom report` and in `--live`. Arrivals are now split into runs at gaps longer than `max(1 s, 20 × the median inter-arrival)`, except a gap a stall window explains (a closed-loop client sends nothing while the server is frozen), and each run is scored against its own schedule origin. The same two runs now give a truth p99 of about 5 ms. For a single run the numbers are unchanged.
- `--live` gave a ✓ or ✗ for a stall window that no traffic preceded. It now prints "stall window passed before any traffic arrived; start a fresh phantom for each run" unless there were at least 20 arrivals in the interval just before the window.

#### Changed

- The `next:` hint printed by `phantom demo` now writes the report to `--html /tmp/phantom-report.html` instead of `out.html` in the current directory.
- The example report moved from `out.html` at the repository root to `phantom/docs/examples/report-c1.html`.
- `phantom report` prints the number of runs and, when there is more than one, a plain-words line with the gaps ("N separate runs found (gaps of X s); truth is computed per run. For a clean result use a fresh phantom per run."). It also prints the arrival rate measured from the log (the median inter-arrival in the longest run) and warns, with both numbers, when it differs from `--rate` by more than 20%. A log that ends mid-JSON, as one still being written does, is read up to its last whole line, with a note. The terminal chart is at most 100 columns wide.
- `--live` shows the current run only: the two p99s, the header (`t` is the time since the run began, with the total time dim on the right once a second run has begun) and the two timeline rows, which are scaled from the run's start to now rather than to the whole uptime, so a 0.5 s freeze in a 10 s run is about 5% of the row. A column is never narrower than 100 ms; when the run is longer than the width allows, buckets are merged by their maximum, and a column holding a bucket with under 10% of the run's median arrivals shows `▁` rather than being hidden. With no arrival for 2 s (and no stall window running) it shows the last result dim, titled "(last run)", and "idle: waiting for traffic" in place of a verdict.
- Library: `phantom::report` gains `find_runs`, `gap_starts_run`, `run_gap_threshold_ns`, `median_gap_ns`, `open_loop_truth_by_run`, `Run` and the `Report` fields `runs`, `measured_rate_per_s` and `ignored_incomplete_line`; `phantom::live` gains `Snapshot::runs`, `current_run` and `is_idle`. `Snapshot::numbers` and `intended_rate_per_s` cover the current run.

#### Unchanged

- The JSONL log format, every server flag, and the truth formula for a single contiguous run. No new dependencies.

### [0.2.0] - 2026-10-22

#### Added

- `phantom demo`: coordinated omission in one command, in about 12 s and in-process. A naive client (tailbench's closed-loop runner: one request at a time, timed from its actual send) and then tailbench's open-loop runner, each against a fresh phantom on an ephemeral loopback port with 1 ms service and a 2000:500 freeze, both asked for 100 requests/s. Each run's final panel and the summary show the same two numbers, the tool's own p99 and the open-loop truth from that run's log, and the same ratio (✓ within 10%); both logs are kept for `phantom report`.
- `--live`: on a terminal, redraws a panel on stderr about 10 times a second: requests arriving over the whole run with the frozen windows underneath, then a "p99 latency" block of two bars on one linear scale (eighth-block ends): "your tool ≈ reports", phantom's estimate from server time, which is what a tool timing from its actual sends sees, and "users waited" (open-loop truth at the rate measured outside the freeze), red when more than 3× the other, else both green; values to 3 significant figures. Once the first freeze has ended, a verdict says whether the load tool kept sending through it. At most 80 columns wide on any terminal; colour unless `NO_COLOR` is set; plain numbers instead of bars below 50 columns; text wraps, never truncated. At shutdown the final panel stays, with a one-line summary. Off a terminal it prints nothing extra.
- `phantom report --rate R LOG.jsonl [--html OUT.html]`: open-loop truth (from the intended send time, keyed by seq) and server time for every request, as a block chart against intended send time and nearest-rank p50, p99, p99.9 and max; for streams, TTFT truth and each request's largest token gap against its mean ITL. `--html` writes one self-contained page: the two p99s and the verdict up top, an inline SVG timeline, the percentile table and the manifest's script, in light and dark.
- Library: `phantom::live` (the panel's counters, its pure renderer and the freeze verdict), `phantom::report` (`schedule_origin`, `open_loop_truth`, `nearest_rank` and the report), and `Phantom::monitor`.
- Docs: the measured overhead of server time over the scripted time (about 2.6 ms at p50, from tokio's millisecond timer) in the truth log reference.

#### Changed

- tailbench-phantom now depends on tailbench 0.1.0, for `phantom demo`'s runners.

#### Unchanged

- The JSONL log format and every server flag; the server runs as before when `report` or `demo` is not the first argument.

### [0.1.0] - 2026-10-22

#### Added

- Scripted fake OpenAI-compatible server, as the `phantom` binary and the `phantom` library: `POST /v1/chat/completions` and `POST /v1/completions`, plus `GET /healthz` and `GET /health`. Service times are fixed or lognormal, each a pure function of (seed, seq), and stall windows hold requests that arrive inside them.
- Streaming: Server-Sent Events, one chunk per token, on a scripted time to first token and inter-token latency with an absolute token schedule; `--role-chunk` for when a chat stream's role chunk is sent, `--token-text` for the token text, and a token limit read from `max_completion_tokens` or `max_tokens`.
- Truth log: a JSONL manifest line, then one line per request with its arrival, hand-off and stall-wait times, `client_id` from the `x-request-id` header, and per-token scheduled and hand-off times for streams.
- Abandoned requests: a request whose client goes away before its response is finished still gets a line, marked `"completed": false`, with `abandoned_ns`.
- vLLM-compatible stream end: the last token's chunk carries `finish_reason`, and a usage chunk with `"choices": []` follows only when `stream_options.include_usage` is `true`.
