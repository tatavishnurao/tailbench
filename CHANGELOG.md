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
