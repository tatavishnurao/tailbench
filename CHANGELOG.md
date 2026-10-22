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

### [0.1.0] - 2026-10-22

#### Added

- Scripted fake OpenAI-compatible server, as the `phantom` binary and the `phantom` library: `POST /v1/chat/completions` and `POST /v1/completions`, plus `GET /healthz` and `GET /health`. Service times are fixed or lognormal, each a pure function of (seed, seq), and stall windows hold requests that arrive inside them.
- Streaming: Server-Sent Events, one chunk per token, on a scripted time to first token and inter-token latency with an absolute token schedule; `--role-chunk` for when a chat stream's role chunk is sent, `--token-text` for the token text, and a token limit read from `max_completion_tokens` or `max_tokens`.
- Truth log: a JSONL manifest line, then one line per request with its arrival, hand-off and stall-wait times, `client_id` from the `x-request-id` header, and per-token scheduled and hand-off times for streams.
- Abandoned requests: a request whose client goes away before its response is finished still gets a line, marked `"completed": false`, with `abandoned_ns`.
- vLLM-compatible stream end: the last token's chunk carries `finish_reason`, and a usage chunk with `"choices": []` follows only when `stream_options.include_usage` is `true`.
