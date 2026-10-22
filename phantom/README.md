# phantom

A deterministic fake OpenAI-compatible server that logs ground truth for every request.

A latency harness can only be audited against something that knows what really happened. phantom follows a script fixed at startup and writes what it did for each request to a JSONL log, so a harness's measurements can be compared request by request with the truth.

Not published; it lives in the tailbench workspace as a test fixture.

## Usage

```sh
cargo run -p phantom -- --listen 127.0.0.1:8080 --seed 1 --service-ms 20 --stall 2000:500 --log /tmp/gt.jsonl
```

| flag | meaning |
|:---|:---|
| `--listen ADDR` | socket address to bind (default `127.0.0.1:8080`) |
| `--seed N` | u64 seed for lognormal service times (default 0) |
| `--service-ms MS` | fixed service time, whole milliseconds > 0 |
| `--lognormal MEDIAN_MS:SIGMA` | service time `MEDIAN_MS * exp(SIGMA * Z)`, Z standard normal; both finite and > 0 |
| `--stall START_MS:LEN_MS` | freeze window `[START_MS, START_MS + LEN_MS)` after t0; repeatable; length > 0; windows must not overlap |
| `--ttft-ms MS` | streaming time to first token, whole milliseconds > 0 (default: the service-time spec) |
| `--ttft-lognormal MEDIAN_MS:SIGMA` | lognormal streaming time to first token |
| `--itl-ms MS` | streaming inter-token latency, whole milliseconds > 0 (default: 10) |
| `--itl-lognormal MEDIAN_MS:SIGMA` | lognormal streaming inter-token latency |
| `--output-tokens N` | tokens per streamed response, > 0 (default 32); a request's smaller token limit wins (see "Streaming") |
| `--role-chunk WHEN` | when a chat stream's role chunk is sent: `with-first-token` (default) or `at-arrival` |
| `--token-text TEXT` | the text every streamed token carries, not empty (default `" tok"`); one chunk is one token |
| `--log PATH` | ground-truth log, created or truncated |

Exactly one of `--service-ms` or `--lognormal` is required. At most one of each `--ttft-*` and `--itl-*` pair may be given. Invalid specs exit non-zero with a message before anything is bound or written. SIGINT or SIGTERM shuts down gracefully: in-flight requests, including streams, are answered, then the log is flushed.

Endpoints:

- `POST /v1/chat/completions` returns a fixed non-streaming chat completion (`id` depends on seq; nothing else varies), or an event stream when the body has `"stream": true`.
- `POST /v1/completions` does the same with the completions shapes: `text_completion` objects with `choices[0].text`.
- `GET /healthz` and `GET /health` return 200 with the same body, are not logged, and do not start the clock.

## Streaming

```sh
cargo run -p phantom -- --service-ms 1 --ttft-ms 100 --itl-ms 20 --output-tokens 5 --log /tmp/gt.jsonl
curl -N -H 'content-type: application/json' -d '{"stream":true}' http://127.0.0.1:8080/v1/chat/completions
```

A body with `"stream": true` (the JSON boolean) gets `text/event-stream`, one `data: {...}` event per chunk, each followed by a blank line:

- **chat:** a `chat.completion.chunk` with `delta.role` `"assistant"` (the role chunk), then one chunk per token with `delta.content` set to the token text (`--token-text`, default `" tok"`). The last token's chunk has `finish_reason` `"length"`; the others have `null`.
- **completions:** one `text_completion` chunk per token with `choices[0].text` set to the token text, the last with `finish_reason` `"length"`. There is no role chunk.
- **end:** if the body has `"stream_options": {"include_usage": true}` (the JSON boolean), one more chunk follows with `"choices": []` and `usage` (`prompt_tokens` 8, `completion_tokens` = tokens sent). Then `data: [DONE]`.

This ending follows vLLM 0.32.0's server: the finish reason rides on the last token's chunk, and the usage chunk, without choices, is sent only when the request asks for it ([`completion/serving.py` L458–L492](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/entrypoints/openai/completion/serving.py#L458-L492)). phantom does not send per-chunk usage (`stream_options.continuous_usage_stats`), which vLLM adds to every chunk when it is asked for along with `include_usage`.

Only `stream`, `max_completion_tokens`, `max_tokens` and `stream_options.include_usage` are read from the body; everything else is ignored. Without `"stream": true`, including when the body is not JSON, both endpoints take the non-streaming path and log exactly the old fields.

Tokens per response: `min(limit, --output-tokens)`, where the limit is `max_completion_tokens` if it is a whole number, else `max_tokens` if that is; with neither, `--output-tokens`. So when both are whole numbers, `max_completion_tokens` wins. A limit of 0 still gets one token: a stream always carries at least one.

Timing, on the server's own clock:

- The schedule is absolute. Token 0 is scheduled at `arrival + scripted_ttft_ns`, and token k at token k−1's *scheduled* time + `scripted_itl_ns` for that gap, never at its actual hand-off. phantom sleeps until each scheduled time, so timer overshoot on one token does not delay the next: it does not accumulate over a stream. Each token's scheduled time is logged in `scheduled_token_ns`, and `token_ns[k] >= scheduled_token_ns[k]` for every k.
- If a token's scheduled time falls inside a stall window, that token and every later scheduled time move later by `window end − scheduled time` (logged in `token_stall_wait_ns` for that token, 0 for the others). The gaps scripted after the stall are kept as they are. A token moved to a window's end is not moved again, even if another window starts there: each window is applied separately, as for non-streaming requests.
- The chat role chunk is sent according to `--role-chunk`. With `with-first-token` (default), phantom hands it off just before token 0, so a client that times the first chunk and one that times the first content chunk see the same TTFT. With `at-arrival`, phantom hands it off as soon as the request arrives. That is before TTFT, and also before any stall, so the two clients see different TTFTs. Either way, chat lines log the hand-off time in `role_chunk_ns`.
- `scripted_ttft_ns` is a pure function of (seed, seq), and each `scripted_itl_ns` of (seed, seq, token index). Both use splitmix64 and Box-Muller like the service time, with their own keys, so they never reuse the service-time draw or each other's. With no `--ttft-*`, TTFT follows the service-time spec but takes its own draws.

## Script

- **t0** is the arrival of the first chat or completions request, on the server's monotonic clock. All logged times are ns since t0.
- **seq** comes from an atomic counter at arrival: 0, 1, 2, ... with no gaps. Every seq gets exactly one log line, completed or abandoned (see "Abandoned requests").
- **scripted_service_ns** is a pure function of (seed, seq): splitmix64 feeds Box-Muller for lognormal; fixed is `ms * 1e6`.
- **stall_wait_ns** is `window_end - arrival_ns` if arrival falls in a stall window (start inclusive, end exclusive), else 0.
- A non-streaming request is held until `arrival + stall_wait_ns`, then sleeps `scripted_service_ns`, then gets its response. Streams follow the token schedule in "Streaming" above.

## Log format

The first line is the manifest; then one line per chat or completions request, in the order responses were finished (not seq order):

```json
{"type":"manifest","phantom_version":"0.1.0","seed":1,"service":{"kind":"fixed","ms":20},"stalls":[{"start_ms":2000,"len_ms":500}],"ttft":{"kind":"fixed","ms":20},"itl":{"kind":"fixed","ms":10},"output_tokens":32,"role_chunk":"with-first-token","token_text":" tok","listen":"127.0.0.1:8080","start_unix_ns":1792507757152789210,"times":"ns since t0, the arrival of the first chat or completions request, on the server's monotonic clock"}
{"type":"req","seq":2,"arrival_ns":2161907075,"scripted_service_ns":20000000,"stall_wait_ns":338092925,"sent_ns":2522484756,"client_id":2}
```

The manifest's `ttft` is the TTFT spec in effect: the service-time spec unless `--ttft-*` was given. Its `role_chunk` is the `--role-chunk` mode in effect, `with-first-token` or `at-arrival`, and its `token_text` the `--token-text` in effect.

`client_id` is the request's `x-request-id` header when it parses as a u64 (decimal); it is omitted when the header is absent or is not a u64. It lets a load generator match its own request index to the log line.

A streamed request's line has the same fields plus the per-token ground truth. Here it is from `--ttft-ms 100 --itl-ms 20 --output-tokens 5`:

```json
{"type":"req","seq":0,"arrival_ns":178,"scripted_service_ns":180000000,"stall_wait_ns":0,"sent_ns":181670630,"stream":true,"endpoint":"/v1/chat/completions","scripted_ttft_ns":100000000,"scripted_itl_ns":[20000000,20000000,20000000,20000000],"token_ns":[102320589,121863212,141670293,162100709,181634388],"scheduled_token_ns":[100000178,120000178,140000178,160000178,180000178],"token_stall_wait_ns":[0,0,0,0,0],"role_chunk_ns":102295810,"output_tokens":5}
```

| field | meaning |
|:---|:---|
| `stream` | always `true` on a streamed line; absent otherwise |
| `endpoint` | `/v1/chat/completions` or `/v1/completions` |
| `scripted_ttft_ns` | scripted time to first token |
| `scripted_itl_ns` | scripted gap before each token after the first (`output_tokens` − 1 entries) |
| `token_ns` | when each token's chunk was handed to the response stream, ns since t0 |
| `scheduled_token_ns` | when each token was scheduled, after any stall shift (see "Streaming"); `token_ns[k] >= scheduled_token_ns[k]` |
| `token_stall_wait_ns` | how far a stall moved each token's scheduled time (later tokens move with it, but log 0) |
| `role_chunk_ns` | chat only: when the role chunk was handed off; absent on completions lines |
| `output_tokens` | tokens the response was scripted to carry, `min(limit, --output-tokens)` (see "Streaming"); on a completed line, the tokens sent |
| `sent_ns` | when the `[DONE]` event was handed off |
| `scripted_service_ns` | `scripted_ttft_ns` + the sum of `scripted_itl_ns` |
| `stall_wait_ns` | the sum of `token_stall_wait_ns` |

So `sent_ns - arrival_ns >= stall_wait_ns + scripted_service_ns` holds for every completed line, streamed or not.

### Abandoned requests

If the client goes away before its response is finished, phantom still writes the request's line, marked `"completed": false`. This covers a disconnect or a timeout on a non-streaming request, and a closed connection mid-stream. A drop guard writes the line when the request's handler (or, for a stream, its token task) is dropped without finishing. The line has `abandoned_ns`, the time phantom saw the drop, in ns since t0, instead of `sent_ns`, plus the fields known by then. Completed lines have no `completed` field, so they are unchanged.

A non-streaming request that timed out during a stall (`--service-ms 20 --stall 0:500`, client timeout 100 ms):

```json
{"type":"req","seq":0,"completed":false,"arrival_ns":111,"scripted_service_ns":20000000,"stall_wait_ns":499999889,"abandoned_ns":101202294,"client_id":3}
```

A chat stream whose client closed the connection after 150 ms (`--ttft-ms 100 --itl-ms 20 --output-tokens 5 --role-chunk at-arrival`). It was abandoned after 3 of its 5 tokens:

```json
{"type":"req","seq":1,"completed":false,"arrival_ns":186168718,"scripted_service_ns":140000000,"stall_wait_ns":0,"abandoned_ns":338937276,"stream":true,"endpoint":"/v1/chat/completions","scripted_ttft_ns":100000000,"scripted_itl_ns":[20000000,20000000],"token_ns":[288344348,307890273,328403344],"scheduled_token_ns":[286168718,306168718,326168718],"token_stall_wait_ns":[0,0,0],"role_chunk_ns":186309651,"output_tokens":5}
```

- On an abandoned stream, `token_ns`, `scheduled_token_ns` and `token_stall_wait_ns` hold only the tokens handed off before the drop, and `scripted_itl_ns` holds one entry fewer. `scripted_service_ns` and `stall_wait_ns` are the sums over those tokens. `output_tokens` is still the scripted count. `role_chunk_ns` is absent if the role chunk was never handed off.
- While waiting for a token's scheduled time, the stream task also watches for the client to go away, so the drop is seen when hyper sees the connection close, not at the next token. A client that has read k events therefore leaves k token entries on a completions stream. On chat it leaves k − 1, because the first event is the role chunk.
- A request dropped before its body was read has no `scripted_service_ns` or `stall_wait_ns`, since phantom does not yet know whether it is a stream. It also has no streaming fields.

Records go through a channel to a dedicated writer thread, so handlers do not wait on file I/O. Anything written is flushed within 100 ms, and again at shutdown.

## LIMITS

- **sent_ns is handler completion, not wire time.** It is taken after the response body is built but before hyper writes it to the socket; the time to write the response and anything in the kernel or network is not in it.
- **arrival_ns is handler entry, not wire time.** It is taken when the handler starts, after hyper has parsed the request headers. Time spent accepting the connection and reading headers comes before it and is not logged.
- **No concurrency limit and no queueing model.** Every request is handled concurrently and independently: requests do not queue behind each other, so load alone never increases latency. Only the script does.
- **Sleeps overshoot.** tokio timers have millisecond resolution, so a request is held for at least its scripted time and typically about 1-3 ms more. The log records both the scripted time and sent_ns, so the overshoot is visible, never hidden.
- **Tested on loopback only.** The tests and smoke test use 127.0.0.1. Nothing stops binding another address, but behaviour over a real network has not been measured.
- **Lognormal values may differ across platforms.** They use `f64::ln`, `cos` and `exp`, which can differ in the last bit between libm implementations; on the same platform and build they are identical.
- **A log write error does not stop serving.** If writing the log fails, phantom keeps answering requests but stops logging them; the error is reported at shutdown and the exit status is non-zero.
- **The log is flushed, not fsynced.** A clean shutdown leaves it complete; a host crash can lose the last unsynced writes.
- **Overlapping stalls are refused,** not merged; windows that only touch are allowed, and each one is applied separately.
- **The request body is read only for `stream`, `max_completion_tokens`, `max_tokens` and `stream_options.include_usage`.** It is read (up to 16 MiB), and everything else in it is ignored: any prompt gets the same response.
- **A stream reports usage only once, at the end, and only when asked.** Without `"stream_options": {"include_usage": true}` a stream carries no `usage`, as on vLLM's server. phantom never sends the per-chunk usage that vLLM sends for `continuous_usage_stats`.
- **Chunk hand-off time is not wire time.** `token_ns` and a stream's `sent_ns` are taken immediately before each chunk is handed to the response stream. hyper's write to the socket, the kernel and the network come after them.
- **One chunk is one "token", with fixed text.** Every token is the same chunk text, `--token-text` (default `" tok"`). A client that re-tokenizes the text, rather than counting chunks, counts the tokens that text encodes to in its tokenizer, which may not be one per chunk; choose a `--token-text` that is one token in that tokenizer.
- **tokio timers have 1 ms granularity.** Each token is handed off at or after its scheduled time, typically about 1-2 ms after it (`token_ns − scheduled_token_ns`). Because the schedule is absolute, this overshoot does not accumulate. For a 64-token stream at a 10 ms ITL, the largest overshoot was 1.5–2.0 ms over 3 runs; the old schedule, which counted each gap from the previous hand-off, drifted 69–73 ms behind the absolute schedule by the last token. The cost is that an *actual* gap can be shorter than its scripted ITL, when the previous token overshot and the current one did not. An ITL below about 5 ms is not meaningful.
- **An abandoned request's `abandoned_ns` is when phantom saw the drop, not when the client left.** A non-streaming handler is dropped when hyper reads the closed connection. A stream notices when its response body is dropped. A client that half-closes its socket but keeps reading is not seen as gone. A connection that closes before hyper has parsed the request headers never reaches a handler, gets no seq, and has no line.

## Audit results

`tests/audit.rs` runs tailbench against an in-process phantom over HTTP on 127.0.0.1 and compares tailbench's percentiles with truth from phantom's log. Each scenario sends 100 requests/s for 10 s; phantom has a fixed 1 ms service time. The tests are `#[ignore]` because they depend on the machine:

```sh
cargo test --release -p phantom --test audit -- --ignored --nocapture --test-threads=1
```

Truth for request `i` is `sent_ns_i - (origin + i * period)`, where `origin = min over i of (arrival_ns_i - i * period)`. That is the least-delayed request, so the origin carries the smallest send-to-handler delay of the run, not the delay of one particular request. Requests are keyed by `client_id`. Scenario C is checked against scenario B's truth, from the same process. Percentiles of truth use nearest rank, like tailbench's histogram.

Pass criteria: in open loop (A, B), |delta| <= 3 ms at p50 and <= 5 ms at p99, p99.9 and max. In closed loop (C), tailbench's p99 must be below 5% of B's truth p99.

Machine: Intel(R) Core(TM) i7-14650HX, kernel 6.6.87.2-microsoft-standard-WSL2, rustc 1.94.1 (e408947bf 2026-03-25), tokio 1.53.3, release build (opt-level 3), on AC power, Windows Balanced power plan. The machine line comes from tailbench's manifest; it records no tokio version, so that one is from `Cargo.lock`.

Range of delta (tailbench − truth, in ms) over 5 runs, release build:

| scenario | p50 | p99 | p99.9 | max |
|:---|---:|---:|---:|---:|
| A. control, open loop | +0.69 to +1.39 | +0.71 to +1.45 | — | +0.65 to +3.51 |
| B. stall 2000:500, open loop | +0.66 to +1.14 | +0.80 to +2.27 | +0.83 to +1.54 | +0.82 to +1.55 |
| C. stall 2000:500, closed loop, against B's truth | — | −400.70 to −392.57 | — | −9.41 to −1.01 |

These ranges are the observed min–max over the 5 runs, not bounds: with 5 runs a new run falls outside the range about a third of the time by chance, and the bounds that do hold are the pass criteria above.

All 15 tests passed. In C, tailbench's closed-loop p99 was 1.07–1.15% of B's truth p99: that is coordinated omission, which the closed-loop runner has by design. C's max is compared with a different run's truth, so it moves by however far into the window that run's stalled request arrived.

In open loop, tailbench reads higher than truth in every cell, by 0.65 to 3.51 ms. The largest, A max +3.51 ms, came in a run whose scheduler lag peaked at 6.05 ms. Truth cannot see part of what tailbench measures (first item below), so a positive delta of this size is expected.

Limits of the audit:

- **Truth cannot see client-side serialization or socket-read time.** sent_ns is taken in the handler before the response is written. tailbench's latency also includes hyper writing the response, the loopback trip back, and reqwest reading the body. Truth is also low by the smallest send-to-handler delay, which the origin absorbs.
- **tokio timers have 1 ms granularity**, on both sides. tailbench's sends wake up late by up to about 2 ms (its scheduler lag), and phantom's 1 ms sleep overshoots. Both are measured, but they limit how small a delta can be resolved.
- **Loopback only.** Client and server share one machine and one kernel. Nothing here measures a real network.
- **Request order is assumed.** Both tailbench runners call the request closure in schedule order, so request index `i` (sent as `x-request-id`) is send `i` and its intended time is `i * period`. Scenario C also assumes B is a fair reference: same script and rate, but a different run.

## Audit: oha

`tests/audit_oha.rs` runs [oha](https://github.com/hatoo/oha) 1.16.0 against an in-process phantom: fixed 1 ms service, seed 1, stall `2000:500`, with a fresh phantom for each scenario. oha is an external binary, not a dependency, and this is a report, not a pass/fail gate for oha. The only assertions are that oha exits 0, its JSON parses, and phantom logs at least 100 requests.

```sh
cargo install oha --locked   # without --locked, a dependency needs rustc 1.96 (this machine: 1.94.1)
cargo test --release -p phantom --test audit_oha -- --ignored --nocapture --test-threads=1
```

Each scenario runs, against `http://127.0.0.1:<port>/v1/chat/completions`:

```sh
oha -q 100 -z 10s --no-tui --output-format json -m POST -d '{}' -H 'content-type: application/json' [extra flags] URL
```

| scenario | extra flags |
|:---|:---|
| O1 | none (50 connections, the default; no correction) |
| O2 | `--latency-correction` |
| O3 | `--latency-correction -c 10` |
| O4 | `-c 1` |
| O5 | `-c 1 --latency-correction` |

oha's JSON gives latencies in seconds (`latencyPercentiles.p50`, `p99`, `p99.9`, and `summary.slowest` for the maximum); the test converts them to ms.

**Per-run truth, the primary comparison.** oha sends no request id, so each of its requests is keyed by phantom's `seq` (arrival order) and goes through the same truth formula as tailbench's (`truth_from_log` in `tests/common/mod.rs`). The test also prints **send lateness**, `arrival_ns_i - (origin + i * period)`: how late oha's request `i` reached phantom after its intended send time.

**Cross-run reference, secondary.** This is the truth of tailbench's own open-loop run against the same script and rate, computed once per test process. Over the 3 runs it ranged over p50 4.06–4.24, p99 394.82–403.35, p99.9 484.53–493.15, max 494.51–503.14 ms.

**Percentile convention.** oha 1.16.0 computes `latencyPercentiles` in [`src/result_data.rs`, lines 42–51](https://github.com/hatoo/oha/blob/v1.16.0/src/result_data.rs#L42-L51) (`percentile_iter`, used by `Statistics::new`). It sorts the values and takes the value at zero-based index `(p / 100.0 * values.len() as f64) as usize`: a truncated index, with no interpolation. At n = 1000, p99.9 is index 999 (`99.9 / 100.0 * 1000.0` = `999.0000000000001`), the largest value, which is why oha's p99.9 equals its max. Nearest rank, used everywhere else in these audits, takes index `ceil(q * n) - 1`. At n = 1000 that is one rank lower at p50, p99 and p99.9, so per-run truth is shown both ways. In a stalled run, neighbouring tail values are about one 10 ms period apart.

Results over 3 runs (Intel(R) Core(TM) i7-14650HX, kernel 6.6.87.2-microsoft-standard-WSL2, rustc 1.94.1 (e408947bf 2026-03-25), tokio 1.53.3, release build (opt-level 3), on AC power, Windows Balanced power plan). oha's value, then the min to max of oha − truth, in ms:

| scenario | pct | oha ms | − run truth (nearest rank) | − run truth (oha's index) | − cross-run reference |
|:---|:---|---:|---:|---:|---:|
| O1. 50 conns, uncorrected | p50 | 3.60–3.72 | −0.43 to −0.36 | −0.44 to −0.36 | −0.52 to −0.40 |
|  | p99 | 404.61–407.87 | +9.42 to +10.47 | −0.57 to +0.51 | +1.57 to +13.05 |
|  | p99.9 | 494.77–498.07 | +9.66 to +10.67 | −0.33 to +0.69 | +1.63 to +13.44 |
|  | max | 494.77–498.07 | −0.33 to +0.69 | −0.33 to +0.69 | −8.36 to +3.49 |
| O2. 50 conns, corrected | p50 | 3.60–3.75 | −0.40 to −0.32 | −0.40 to −0.32 | −0.50 to −0.43 |
|  | p99 | 404.69–414.30 | +9.53 to +10.05 | −0.44 to +0.16 | +1.34 to +19.35 |
|  | p99.9 | 495.03–504.06 | +9.43 to +10.09 | −0.55 to +0.10 | +1.88 to +19.52 |
|  | max | 495.03–504.06 | −0.55 to +0.10 | −0.55 to +0.10 | −8.11 to +9.54 |
| O3. 10 conns, corrected | p50 | 3.60–3.78 | −0.42 to −0.28 | −0.42 to −0.29 | −0.51 to −0.28 |
|  | p99 | 403.52–414.50 | +5.56 to +6.35 | −0.21 to +0.15 | +8.57 to +19.68 |
|  | p99.9 | 493.10–503.88 | +9.30 to +9.43 | −0.69 to −0.50 | +8.57 to +19.25 |
|  | max | 493.10–503.88 | −0.69 to −0.50 | −0.69 to −0.50 | −1.41 to +9.30 |
| O4. 1 conn, uncorrected | p50 | 3.59–3.87 | −0.47 to −0.37 | −0.47 to −0.37 | −0.62 to −0.19 |
|  | p99 | 4.23–4.54 | −434.97 to −426.91 | −441.44 to −433.26 | −399.12 to −390.42 |
|  | p99.9 | 493.93–504.28 | +5.64 to +6.11 | −0.71 to −0.14 | +9.40 to +18.70 |
|  | max | 493.93–504.28 | −0.71 to −0.14 | −0.71 to −0.14 | −0.58 to +8.75 |
| O5. 1 conn, corrected | p50 | 3.63–3.94 | −0.37 to −0.27 | −0.38 to −0.27 | −0.60 to −0.12 |
|  | p99 | 433.48–436.39 | +5.79 to +6.50 | −0.81 to +0.08 | +33.04 to +40.45 |
|  | p99.9 | 493.58–494.10 | +5.64 to +6.02 | −0.73 to −0.36 | +0.92 to +9.47 |
|  | max | 493.58–494.10 | −0.73 to −0.36 | −0.73 to −0.36 | −9.07 to −0.48 |

These ranges are the observed min–max over the 3 runs, not bounds: with only 3 runs a new run falls outside the range about half the time by chance, and this audit sets no pass criteria at all — it asserts only that oha ran, its JSON parsed, and phantom logged at least 100 requests.

Send lateness (arrival − intended, in ms), min to max over the 3 runs:

| scenario | p50 | p99 | max | requests later than one period (of 1000) |
|:---|---:|---:|---:|---:|
| O1 | 0.65–0.69 | 1.3–1.4 | 1.8–12.6 | 0–1 |
| O2 | 0.62–0.74 | 1.2–1.4 | 1.9–5.0 | 0 |
| O3 | 0.60–0.76 | 297.7–309.1 | 393.9–405.0 | 40–42 |
| O4 | 0.65–0.86 | 421.7–429.6 | 484.6–494.8 | 75 |
| O5 | 0.69–0.80 | 417.8–420.1 | 484.7–485.1 | 73–75 |

What phantom's log showed (min to max over the 3 runs):

| scenario | requests received | oha "aborted due to deadline" | largest arrival gap ms | arrivals in 50 ms after stall end | requests stalled | max stall_wait ms |
|:---|---:|---:|---:|---:|---:|---:|
| O1 | 1000 | 2–3 | 11.1–21.3 | 4–5 | 50 | 490.2–493.6 |
| O2 | 1000 | 1–2 | 11.2–14.2 | 5 | 50 | 490.9–500.0 |
| O3 | 1000 | 1 | 403.3–414.6 | 45–46 | 10 | 489.8–499.9 |
| O4 | 1000 | 1 | 493.8–504.3 | 13–14 | 1 | 489.9–499.8 |
| O5 | 1000 | 0–1 | 493.5–494.1 | 14 | 1 | 489.8–490.1 |

oha reported 1000 responses with status 200 in every scenario. In every row, oha's p99.9 equals its max.

Read at oha's own index, oha's p50, p99, p99.9 and max are within 0.81 ms of this run's truth in every scenario except O4 p99. Read at nearest rank, the p99 and p99.9 differences (apart from O4 p99) are +5.56 to +10.67 ms. That is about one rank of the stall tail.

Candidate explanations, none of them verified:

- **Connection count.** With 50 connections, the 50 requests stalled in the 500 ms window (500 ms × 100/s) take exactly 50 connections, so arrivals keep flowing (largest gap 11–21 ms; at most 1 of 1000 requests later than one period). With 10 or 1 connection, every connection is held by the stall, and arrivals stop for about 400–505 ms (40–42 late requests at `-c 10`, 73–75 at `-c 1`). Then a burst follows: 45–46 arrivals in the 50 ms after the stall at `-c 10`.
- **Rate limiter semantics.** At `-c 1`, uncorrected (O4), the requests that could not be sent during the stall are measured from when they were actually sent, so p99 reads 4.2–4.5 ms against this run's truth of 431–439 ms (coordinated omission). With `--latency-correction` (O5), p99 at oha's index is within 0.81 ms of this run's truth. How `-q` schedules sends that are already late is oha's own design, and this audit did not examine it.
- **Burst after the stall.** At `-c 10` the backlog arrives at once after the stall, so the response times right after the stall depend on how oha catches up. Requests that arrive together may be permuted relative to send order, which seq-keyed truth cannot detect (see Limits).
- **Phase of the schedule against the stall.** phantom's t0 is oha's first arrival. Whether a request lands at the very start of the window decides whether the longest stall_wait is about 490 or 500 ms. That would move the max and tail cells by about one 10 ms period between runs. It would account for much of the spread in the cross-run column, and it cannot affect the per-run columns, which compare oha with truth from the same run.
- **Requests aborted at the deadline.** These are not in oha's percentiles. phantom logs only requests that reached its handler: an aborted one that did leaves a `"completed": false` line, and truth is computed over completed lines only. In every scenario, phantom's `completed: false` count was at most oha's "aborted due to deadline" count, and equal only at `-c 1`. The difference is requests cancelled before they reached a handler.

Limits:

- **The reference is intended-schedule truth from tailbench's own run,** not an independent measurement of oha's run. oha sends no per-request id. Per-run truth matches its requests to log lines by arrival order (`seq`), assuming oha sends in schedule order and arrival order equals send order. Inside a post-stall burst, requests that arrive together may be permuted. That changes individual truth values, but by no more than the burst's spread.
- **Loopback only:** oha, tailbench and phantom share one machine and one kernel.
- **Corrected latency is oha's own definition.** `oha --help` (1.16.0) describes it as: "Correct latency to avoid coordinated omission problem. It's ignored if -q is not set."
- **Release build, one machine.** These numbers come from one machine; other machines and debug builds were not measured.
- **Different percentile conventions.** oha truncates the index and nearest rank rounds it up (see above). Comparing at nearest rank alone would show one rank of difference as a measurement difference.

## Audit: GuideLLM

`tests/audit_guidellm.rs` runs [GuideLLM](https://github.com/vllm-project/guidellm) 0.8.1 against an in-process phantom that streams. It compares GuideLLM's time to first token (TTFT), inter-token latency (ITL) and end-to-end latency with phantom's per-token ground truth. This is a report, not a pass/fail gate for GuideLLM. The only assertions are that GuideLLM exits 0, its JSON parses, and phantom logs at least 20 requests.

GuideLLM is an external Python tool. It is installed in a virtual environment outside the repo, and the repo has no Python files or Python dependencies:

```sh
mkdir -p ~/audit-tools && cd ~/audit-tools
uv venv guidellm-venv
uv pip install --python guidellm-venv "guidellm[recommended]"   # installed GuideLLM 0.8.1
```

Run the audit with:

```sh
GUIDELLM_BIN=~/audit-tools/guidellm-venv/bin/guidellm \
    cargo test --release -p phantom --test audit_guidellm -- --ignored --nocapture --test-threads=1
```

Without `GUIDELLM_BIN`, or if it does not name a file, the scenarios fail with a message that says so. The test also has three tests that run without GuideLLM: the `GUIDELLM_BIN` check, the stream-truth helper on a hand-written log, and the replica of GuideLLM's percentile method on values computed by GuideLLM itself.

phantom: `--service-ms 1 --ttft-ms 100 --itl-ms 20 --output-tokens 64`, seed 1, a fresh phantom for each scenario. Each scenario runs GuideLLM for 30 s, in a temporary directory:

```sh
guidellm run \
  --backend '{"kind":"openai_http","target":"http://127.0.0.1:<port>","model":"phantom"}' \
  --profile <profile> --constraint kind=max_duration,seconds=30 \
  --data kind=synthetic_text,prompt_tokens=16,output_tokens=32 \
  --tokenizer kind=huggingface_auto,model=gpt2 --seed kind=static,value=42 \
  --output kind=json,path=<tmp>/guidellm.json --disable-console-interactive
```

| scenario | `--profile` | phantom |
|:---|:---|:---|
| G1 | `kind=constant,rate=5` | no stall |
| G2 | `kind=constant,rate=5` | `--stall 10000:2000` |
| G3 | `kind=synchronous` (closed loop) | `--stall 10000:2000` |
| G4 | `kind=constant,rate=5` | no stall, `--role-chunk at-arrival` |

Notes on the command:

- **GuideLLM's own health check and token limit.** The backend sets nothing but its target and model. GuideLLM's health check requests `GET /health`, which phantom serves like `/healthz`. On `/v1/chat/completions`, GuideLLM 0.8.1 puts the data's output length in `max_completion_tokens` ([`request_handlers.py` L1179–L1189](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/backends/openai/request_handlers.py#L1179-L1189)), which phantom reads (see "Streaming"), so every response carried 32 tokens.
- **The tables come from runs with two workarounds,** made before phantom served `/health` or read `max_completion_tokens`: the backend had `"validate_backend": false` and `"extras": {"body": {"max_tokens": 32}}`. One run without them is in "Re-run without the workarounds", below the tables.
- **gpt2 and `" tok"`.** gpt2 encodes phantom's token text `" tok"` as 2 tokens (ids 284 and 74, `" to"` and `"k"`), not 1. That does not affect the counts here: GuideLLM takes a response's output tokens from the server's `usage.completion_tokens` when the server sends it ([`request_stats.py` L237–L246](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/base/request_stats.py#L237-L246)), and phantom reports one per chunk. The tokenizer only sizes the synthetic prompts.

### What GuideLLM measures

Read from the installed source. Each file is byte-identical to the same file at the `v0.8.1` tag, so the links match the quoted lines.

**(a) TTFT runs from the request's header write to the first SSE line with non-empty content.** It is defined in [`schemas/base/request_stats.py` L268–L273](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/base/request_stats.py#L268-L273):

```python
        first_token = self.first_token_iteration
        start = self.info.timings.request_start
        if first_token is None or start is None:
            return None

        return 1000 * (first_token - start)
```

`request_start` is set by an httpx trace hook when the request headers start to be written ([`http.py` L424–L432](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/backends/openai/http.py#L424-L432)). `first_token_iteration` is the wall-clock time (`time.time()`) of the first SSE line for which the handler reports a token, in [`backends/openai/http.py` L367–L380](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/backends/openai/http.py#L367-L380):

```python
                    iterations = request_handler.add_streaming_line(chunk)
                    if iterations is None or iterations <= 0 or end_reached:
                        end_reached = end_reached or iterations is None
                        if end_reached:
                            # Break eagerly once the handler signals completion
                            # (e.g. "data: [DONE]" or "response.completed").
                            # Using continue instead would hang on servers that
                            # keep the HTTP/2 stream open after the last event.
                            break
                        continue

                    if request_info.timings.first_token_iteration is None:
                        request_info.timings.first_token_iteration = iter_time
                        request_info.timings.token_iterations = 0
```

For chat, a line counts only if its `delta.content` (or reasoning, or tool call) is truthy ([`backends/openai/request_handlers.py` L1310–L1312](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/backends/openai/request_handlers.py#L1310-L1312)). phantom's role chunk has `"content": ""`, so it never starts TTFT:

```python
        if content := delta.get("content"):
            self.streaming_texts.append(content)
            updated = True
```

**(b) ITL is per request: (last token line − first token line) / (output tokens − 1).** It is not computed from per-chunk gaps. See [`schemas/base/request_stats.py` L343–L354](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/base/request_stats.py#L343-L354):

```python
        first_token = self.first_token_iteration
        last_token = self.last_token_iteration
        output_tokens = self.output_tokens
        if (
            first_token is None
            or last_token is None
            or output_tokens is None
            or output_tokens <= 1
        ):
            return None

        return 1000 * (last_token - first_token) / (output_tokens - 1)
```

Its distribution weights each request by `output_tokens − 1` ([`benchmark/schemas/metrics.py` L1466–L1470](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/benchmark/schemas/metrics.py#L1466-L1470)). So the JSON's ITL `count` is the number of gaps, but every value in it is one request's mean:

```python
            inter_token_latency_ms=StatusDistributionSummary.from_values_function(
                function=lambda req: (
                    req.inter_token_latency_ms or 0.0,
                    (req.output_tokens or 1.0) - 1.0,
                ),
```

End-to-end: `request_latency` is `request_end − request_start` in seconds, where `request_end` is taken after the stream ends ([`request_stats.py` L117–L122](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/base/request_stats.py#L117-L122), [`http.py` L395](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/backends/openai/http.py#L395)). `request_scheduled_latency` is `request_end − targeted_start` in seconds ([`request_stats.py` L211–L216](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/base/request_stats.py#L211-L216)). For the constant profile, `targeted_start` is `start_time + index / rate` ([`scheduler/strategies.py` L574–L576](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/scheduler/strategies.py#L574-L576)). GuideLLM did not report it for the synchronous profile. TTFT and ITL are in ms; the test converts both latencies from s to ms.

**(c) Percentiles come from a weighted CDF with numpy's `searchsorted`.** Values are sorted and equal values merged ([`schemas/base/statistics.py` L466–L474](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/base/statistics.py#L466-L474)):

```python
        # Sort values and weights by values
        sort_ind = np.argsort(np_values[:, 0])
        sorted_values = np_values[sort_ind, 0]
        sorted_weights = np_values[sort_ind, 1]

        # Combine any duplicate values by summing their weights
        unique_values, inverse_indices = np.unique(sorted_values, return_inverse=True)
        combined_weights = np.zeros_like(unique_values, dtype=float)
        np.add.at(combined_weights, inverse_indices, sorted_weights)
```

The weights are then normalised to probabilities (L488), and the percentile is the first value whose running sum reaches q ([L114–L121](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/base/statistics.py#L114-L121)):

```python
        cdf_probs = np.cumsum(probabilities)

        return Percentiles(
            **{
                key: pdf[np.searchsorted(cdf_probs, value, side="left"), 0].item()
                for key, value in PERCENTILE_PROBABILITIES.items()
            }
        )
```

With equal weights, this is nearest rank, except where the float running sum falls just short of q. Then it takes the next rank: for 1..150, p50 is 76 (nearest rank: 75), and for 1..10, p90 is 10 (nearest rank: 9). `common::guidellm_percentile` replicates it, and a test checks the replica against values computed by GuideLLM 0.8.1 itself. Every truth below is shown at nearest rank (NR) and at GuideLLM's method (GL). `max` is GuideLLM's `max` field.

**(d) No warmup or cooldown is excluded by default.** Both phases default to an empty `TransientPhaseConfig` ([`schemas/benchmark/profiles/profile.py` L63–L72](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/schemas/benchmark/profiles/profile.py#L63-L72)). With no warmup, the measurement window starts at the first request's start ([`benchmark/schemas/accumulator.py` L182–L189](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/benchmark/schemas/accumulator.py#L182-L189)):

```python
        warmup_active, measure_start = config.warmup.compute_transition_time(
            info=info, state=scheduler_state, period="start"
        )
        if not warmup_active:
            # No warmup, set measure_start to first request start
            self.measure_start = self.request_start
        elif measure_start is not None:
            self.measure_start = measure_start
```

Requests that overlap the window are kept ([`metrics.py` L1255–L1257](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/benchmark/schemas/metrics.py#L1255-L1257)). Requests still in flight at the 30 s deadline are cancelled and reported as `incomplete`, not `successful`. The tables use GuideLLM's `successful` distributions.

### Truth

Truth comes from phantom's log through `tests/common/mod.rs` (`stream_lines`, `stream_schedule_truth`). Only completed lines are used; `"completed": false` lines are counted but kept out.

- **Server TTFT** = `token_ns[0] − arrival_ns`. **Server end-to-end** = `sent_ns − arrival_ns`.
- **ITL truth**, two ways. The **per-request mean** is `(token_ns[last] − token_ns[0]) / (tokens − 1)`, weighted by `tokens − 1` like GuideLLM's. **Pooled gaps** are every consecutive difference of `token_ns`.
- **Intended-schedule truth**, constant profile only. Request `i` is phantom's `seq` `i`, and its intended send time is `origin + i × 200 ms`, with the same origin formula as the other audits (`schedule_origin`: the least-lagged request). Intended TTFT = `token_ns[0] − intended`, intended end-to-end = `sent_ns − intended`, and send lateness = `arrival_ns − intended`.
- **G3** is synchronous: it has no intended schedule. It shows server truth and, as a cross-run reference, G2's intended-schedule truth from the same test process. TTFT is compared with G2's intended TTFT, and GuideLLM's `request_latency` with G2's intended end-to-end.

### Results

3 runs with the two workarounds (see "Notes on the command"), release build. Intel(R) Core(TM) i7-14650HX, kernel 6.6.87.2-microsoft-standard-WSL2, rustc 1.94.1 (e408947bf 2026-03-25), tokio 1.53.3, release build (opt-level 3), on AC power, Windows Balanced power plan; Python 3.13.15, GuideLLM 0.8.1, httpx 0.28.1, numpy 2.5.4, transformers 5.20.2. Every cell is the min to max over the 3 runs, in ms. Deltas are GuideLLM − truth.

**TTFT** (ms)

| scenario | pct | GuideLLM | server truth | − server (NR) | − server (GL) | intended truth | − intended (NR) | − intended (GL) |
|:---|:---|---:|---:|---:|---:|---:|---:|---:|
| G1 | p50 | 101.590–101.597 | 101.115–101.133 | +0.462 to +0.482 | +0.462 to +0.482 | 102.612–102.923 | −1.333 to −1.015 | −1.333 to −1.015 |
|  | p90 | 102.105–102.317 | 101.626–101.802 | +0.477 to +0.515 | +0.477 to +0.515 | 104.422–105.433 | −3.288 to −2.106 | −3.288 to −2.106 |
|  | p99 | 102.406–102.763 | 101.872–102.039 | +0.511 to +0.764 | +0.511 to +0.764 | 123.823–124.203 | −21.653 to −21.398 | −21.653 to −21.398 |
|  | max | 102.565–102.884 | 101.950–102.090 | +0.509 to +0.934 | +0.509 to +0.934 | 125.023–128.438 | −25.873 to −22.139 | −25.873 to −22.139 |
| G2 | p50 | 101.509–101.627 | 101.026–101.194 | +0.371 to +0.499 | +0.371 to +0.499 | 103.066–103.572 | −2.029 to −1.557 | −2.029 to −1.557 |
|  | p90 | 102.253–102.373 | 101.702–101.950 | +0.370 to +0.563 | +0.370 to +0.563 | 122.659–125.439 | −23.174 to −20.406 | −23.174 to −20.406 |
|  | p99 | 1821.340–1828.306 | 1820.636–1827.813 | +0.493 to +0.705 | +0.493 to +0.705 | 1822.369–1830.758 | −2.452 to −0.213 | −2.452 to −0.213 |
|  | max | 2021.941–2027.357 | 2021.335–2027.007 | +0.350 to +0.606 | +0.350 to +0.606 | 2022.269–2030.590 | −3.232 to −0.328 | −3.232 to −0.328 |
| G3 | p50 | 101.700–101.734 | 101.302–101.329 | +0.398 to +0.410 | +0.366 to +0.396 | 103.066–103.572 | −1.853 to −1.332 | −1.853 to −1.332 |
|  | p90 | 102.013–102.343 | 101.571–101.819 | +0.439 to +0.524 | +0.439 to +0.524 | 122.659–125.439 | −23.425 to −20.646 | −23.425 to −20.646 |
|  | p99 | 102.394–103.055 | 101.746–102.589 | +0.395 to +0.648 | +0.395 to +0.648 | 1822.369–1830.758 | −1728.364 to −1719.660 | −1728.364 to −1719.660 |
|  | max | 102.394–103.055 | 101.746–102.589 | +0.395 to +0.648 | +0.395 to +0.648 | 2022.269–2030.590 | −1928.196 to −1919.560 | −1928.196 to −1919.560 |
| G4 | p50 | 101.341–101.524 | 100.980–101.144 | +0.360 to +0.380 | +0.360 to +0.380 | 102.780–103.042 | −1.702 to −1.293 | −1.702 to −1.293 |
|  | p90 | 101.958–102.062 | 101.610–101.716 | +0.346 to +0.401 | +0.346 to +0.401 | 104.079–105.082 | −3.051 to −2.017 | −3.051 to −2.017 |
|  | p99 | 102.222–102.595 | 101.881–102.036 | +0.292 to +0.606 | +0.292 to +0.606 | 124.426–127.264 | −24.670 to −22.098 | −24.670 to −22.098 |
|  | max | 102.258–102.748 | 101.887–102.327 | +0.370 to +0.421 | +0.370 to +0.421 | 124.427–127.794 | −25.066 to −21.680 | −25.066 to −21.680 |

**ITL** (ms)

| scenario | pct | GuideLLM | per-request mean truth | − (NR) | − (GL) | pooled-gap truth | − (NR) | − (GL) |
|:---|:---|---:|---:|---:|---:|---:|---:|---:|
| G1 | p50 | 19.996–19.998 | 19.998–20.000 | −0.002 to −0.001 | −0.002 to −0.001 | 20.220–20.234 | −0.237 to −0.222 | −0.237 to −0.222 |
|  | p90 | 20.018–20.020 | 20.018–20.020 | −0.001 to +0.001 | −0.001 to +0.001 | 20.453–20.542 | −0.522 to −0.434 | −0.522 to −0.434 |
|  | p99 | 20.030–20.033 | 20.026–20.029 | +0.001 to +0.007 | +0.001 to +0.007 | 20.831–20.845 | −0.815 to −0.798 | −0.815 to −0.798 |
|  | max | 20.035–20.050 | 20.030–20.031 | +0.005 to +0.019 | +0.005 to +0.019 | 21.248–29.275 | −9.237 to −1.213 | −9.237 to −1.213 |
| G2 | p50 | 19.997–20.000 | 19.998–20.001 | −0.001 | −0.001 | 20.185–20.231 | −0.233 to −0.185 | −0.233 to −0.185 |
|  | p90 | 20.019–20.022 | 20.021 | −0.002 to 0.000 | −0.002 to 0.000 | 20.520–20.555 | −0.534 to −0.501 | −0.534 to −0.501 |
|  | p99 | 83.891–84.519 | 83.890–84.511 | +0.001 to +0.007 | +0.001 to +0.007 | 20.813–20.874 | +63.034 to +63.644 | +63.034 to +63.644 |
|  | max | 83.916–84.522 | 83.921–84.525 | −0.010 to −0.002 | −0.010 to −0.002 | 2002.835–2020.162 | −1935.639 to −1918.919 | −1935.639 to −1918.919 |
| G3 | p50 | 19.997–19.998 | 19.997–19.998 | −0.001 to 0.000 | −0.002 to −0.001 | 20.137–20.170 | −0.173 to −0.140 | −0.173 to −0.140 |
|  | p90 | 20.017–20.019 | 20.016–20.021 | −0.002 to +0.002 | −0.002 to +0.002 | 20.326–20.372 | −0.355 to −0.307 | −0.355 to −0.307 |
|  | p99 | 84.124–84.153 | 84.127–84.155 | −0.004 to −0.003 | −0.004 to −0.003 | 20.586–20.669 | +63.455 to +63.566 | +63.455 to +63.566 |
|  | max | 84.124–84.153 | 84.127–84.155 | −0.004 to −0.003 | −0.004 to −0.003 | 2008.157–2009.111 | −1924.959 to −1924.033 | −1924.959 to −1924.033 |
| G4 | p50 | 20.000–20.001 | 19.999–20.001 | 0.000 to +0.001 | 0.000 to +0.001 | 20.217–20.218 | −0.217 to −0.216 | −0.217 to −0.216 |
|  | p90 | 20.018–20.023 | 20.019–20.022 | −0.001 to +0.003 | −0.001 to +0.003 | 20.508–20.527 | −0.509 to −0.485 | −0.509 to −0.485 |
|  | p99 | 20.028–20.034 | 20.028–20.030 | 0.000 to +0.004 | 0.000 to +0.004 | 20.808–20.825 | −0.794 to −0.778 | −0.794 to −0.778 |
|  | max | 20.032–20.042 | 20.030–20.033 | 0.000 to +0.008 | 0.000 to +0.008 | 21.076–21.174 | −1.141 to −1.038 | −1.141 to −1.038 |

**End-to-end** (GuideLLM `request_latency`, converted from s to ms)

| scenario | pct | GuideLLM | server truth | − server (NR) | − server (GL) | G2 intended truth (G3 only) | − (NR) | − (GL) |
|:---|:---|---:|---:|---:|---:|---:|---:|---:|
| G1 | p50 | 721.668–721.851 | 721.055–721.167 | +0.556 to +0.684 | +0.556 to +0.684 | — | — | — |
|  | p90 | 722.377–722.531 | 721.646–721.770 | +0.680 to +0.886 | +0.680 to +0.886 | — | — | — |
|  | p99 | 722.729–723.367 | 721.940–722.018 | +0.789 to +1.384 | +0.789 to +1.384 | — | — | — |
|  | max | 722.805–731.256 | 722.043–722.154 | +0.728 to +9.213 | +0.728 to +9.213 | — | — | — |
| G2 | p50 | 721.737–721.833 | 721.085–721.227 | +0.588 to +0.656 | +0.588 to +0.656 | — | — | — |
|  | p90 | 722.680–722.698 | 721.895–721.969 | +0.729 to +0.792 | +0.729 to +0.792 | — | — | — |
|  | p99 | 2702.961–2721.489 | 2702.265–2720.702 | +0.696 to +0.829 | +0.696 to +0.829 | — | — | — |
|  | max | 2703.067–2721.858 | 2702.661–2721.031 | +0.405 to +0.827 | +0.405 to +0.827 | — | — | — |
| G3 | p50 | 721.807–721.919 | 721.119–721.283 | +0.525 to +0.800 | +0.510 to +0.786 | 723.107–723.524 | −1.717 to −1.299 | −1.717 to −1.299 |
|  | p90 | 722.471–722.774 | 721.733–721.905 | +0.738 to +0.869 | +0.738 to +0.869 | 744.249–749.979 | −27.379 to −21.475 | −27.379 to −21.475 |
|  | p99 | 2710.593–2711.637 | 2710.120–2710.524 | +0.473 to +1.112 | +0.473 to +1.112 | 2704.209–2722.325 | −11.438 to +7.427 | −11.438 to +7.427 |
|  | max | 2710.593–2711.637 | 2710.120–2710.524 | +0.473 to +1.112 | +0.473 to +1.112 | 2704.374–2722.938 | −12.050 to +7.263 | −12.050 to +7.263 |
| G4 | p50 | 721.555–721.755 | 721.034–721.180 | +0.521 to +0.575 | +0.521 to +0.575 | — | — | — |
|  | p90 | 722.214–722.372 | 721.602–721.779 | +0.593 to +0.612 | +0.593 to +0.612 | — | — | — |
|  | p99 | 722.550–723.038 | 721.936–722.126 | +0.614 to +0.912 | +0.614 to +0.912 | — | — | — |
|  | max | 722.790–723.524 | 722.003–722.496 | +0.580 to +1.520 | +0.580 to +1.520 | — | — | — |

**Scheduled end-to-end** (GuideLLM `request_scheduled_latency`, s to ms; constant profile only)

| scenario | pct | GuideLLM | intended truth | − intended (NR) | − intended (GL) |
|:---|:---|---:|---:|---:|---:|
| G1 | p50 | 724.589–725.174 | 722.686–722.970 | +1.795 to +2.476 | +1.795 to +2.476 |
|  | p90 | 726.827–727.479 | 725.060–725.244 | +1.695 to +2.419 | +1.695 to +2.419 |
|  | p99 | 745.563–746.993 | 743.762–745.123 | +1.801 to +2.476 | +1.801 to +2.476 |
|  | max | 746.637–750.351 | 744.729–747.884 | +1.907 to +2.467 | +1.907 to +2.467 |
| G2 | p50 | 724.750–725.008 | 723.107–723.524 | +1.427 to +1.643 | +1.427 to +1.643 |
|  | p90 | 745.734–751.705 | 744.249–749.979 | +1.485 to +1.845 | +1.485 to +1.845 |
|  | p99 | 2705.665–2723.984 | 2704.209–2722.325 | +1.456 to +1.839 | +1.456 to +1.839 |
|  | max | 2705.747–2724.730 | 2704.374–2722.938 | +1.373 to +1.793 | +1.373 to +1.793 |
| G4 | p50 | 724.345–724.805 | 722.813–722.963 | +1.383 to +1.863 | +1.383 to +1.863 |
|  | p90 | 725.815–726.613 | 724.338–724.832 | +1.357 to +1.781 | +1.357 to +1.781 |
|  | p99 | 746.076–748.611 | 744.286–747.016 | +1.595 to +1.838 | +1.595 to +1.838 |
|  | max | 746.286–748.969 | 744.490–747.557 | +1.412 to +1.955 | +1.412 to +1.955 |

**Requests and send lateness**

| scenario | GuideLLM successful / incomplete | phantom completed / `completed: false` | lateness p50 ms | lateness p99 ms | lateness max ms |
|:---|---:|---:|---:|---:|---:|
| G1 | 147/4 | 147/4 | 1.364–1.862 | 22.330–23.607 | 23.504–26.650 |
| G2 | 147/4 | 147/4 | 1.863–2.518 | 23.390–29.328 | 23.826–29.882 |
| G3 | 38/1 | 38/1 | — | — | — |
| G4 | 147/4 | 147/4 | 1.709–1.968 | 23.275–26.363 | 23.358–26.929 |

These ranges are the observed min–max over the 3 runs, not bounds: with only 3 runs a new run falls outside the range about half the time by chance, and this audit sets no pass criteria at all — it asserts only that GuideLLM exited 0, its JSON parsed, and phantom logged at least 20 requests.

In G3, the "intended truth" columns are G2's intended-schedule truth, a cross-run reference. In every run, GuideLLM's request counts equalled phantom's: GuideLLM `successful` = phantom completed, and GuideLLM `incomplete` = phantom `"completed": false`. In the constant runs, no request arrived more than one period (200 ms) after its intended send time. In G4, phantom handed off the role chunk 0.109–0.117 ms (p50) after arrival, and the first content token 100.980–101.144 ms (p50) after arrival.

What the tables show:

- **TTFT against server truth.** GuideLLM − server TTFT was +0.29 to +0.93 ms in every cell of every scenario.
- **TTFT against intended truth.** GuideLLM read lower than intended-schedule TTFT: by 1.0–2.0 ms at p50 in the constant runs, and by 21.4–25.9 ms at p99 and max in G1 and G4. Send lateness (arrival − intended) was 1.4–2.5 ms at p50 and 22.3–29.3 ms at p99, with no request later than one period.
- **Role chunk (G4).** With the role chunk on arrival, GuideLLM's TTFT stayed about 100 ms after arrival (p50 101.34–101.52 ms; G1: 101.59–101.60 ms). GuideLLM's TTFT did not count the role chunk, as (a) predicts.
- **ITL.** GuideLLM matched the per-request-mean truth within −0.010 to +0.019 ms in every cell. In G2 and G3, the stalled request's mean was about 84 ms, while the largest pooled gap was 2.00–2.02 s. A per-request mean cannot show a single long gap, by its definition in (b).
- **End-to-end against server truth.** GuideLLM − server was +0.41 to +1.52 ms in every cell, except G1 max: up to +9.21 ms, in one of the 3 runs.
- **Scheduled end-to-end.** GuideLLM `request_scheduled_latency` − intended truth was +1.36 to +2.48 ms in G1, G2 and G4.
- **G3, closed loop.** GuideLLM's TTFT p99 and max were 102.4–103.1 ms, against G2's intended TTFT of 1822–1831 ms (p99) and 2022–2031 ms (max). In G3, GuideLLM sent nothing while a request was outstanding: 38 requests in 30 s, and the stall delayed one of them, mid-stream. With n = 38, p99 and max are the same request. GuideLLM's `request_latency` p90 was 21.5–27.4 ms below G2's intended p90.
- **Percentile method.** Nearest rank and GuideLLM's method gave different values only in G3 at p50 (n = 38), where the running sum lands one rank higher (TTFT 101.302–101.329 ms at nearest rank, 101.304–101.368 ms at GuideLLM's).

### Re-run without the workarounds

Once phantom served `/health` and read `max_completion_tokens`, the audit was run once more, in release, on the same machine, with the backend as in the command above: GuideLLM's own health check and token limit.

- **Exit and tokens.** All 4 scenarios exited 0. Every response carried 32 tokens: GuideLLM's output token count was 32 (mean, min and max) in every scenario, and so was phantom's per completed line.
- **Request counts.** They matched in G1, G3 and G4 (147/4, 38/1, 147/4). In G2, GuideLLM reported 147 successful and 3 incomplete (150 requests in total), and phantom logged 147 completed lines and 4 `"completed": false` lines.
- **Against the 3-run ranges.** 310 of the 453 printed values (truths, GuideLLM values and deltas) fell outside the ranges in the tables above, most by under 0.2 ms. The ranges are not bounds, and none of this is a failure. The values that moved most:
  - **Send lateness** p99 was 16.3 ms in G1 and 4.7–4.8 ms in G2 and G4 (the tables: 22.3–29.3 ms); max was 17.2 ms in G1 and 5.0–5.4 ms in G2 and G4 (23.5–29.9 ms). p50 was 2.0–2.2 ms.
  - **TTFT − intended truth** at p99 and max was −14.3 and −13.8 ms in G1 and −3.9 and −4.3 ms in G4 (the tables: −21.4 to −25.9 ms). G2 at p90 was −2.6 ms (−20.4 to −23.2 ms).
  - **Scheduled end-to-end − intended truth** was +1.9 to +3.0 ms (the tables: +1.4 to +2.5 ms).
  - **G2's stalled requests.** TTFT p99 was 1801.1 ms and max 2002.7 ms (the tables: 1821–1828 and 2022–2027 ms). Server truth moved with them (1800.7 and 2001.9 ms), so GuideLLM − server was +0.39 and +0.74 ms.
  - **Against server truth,** the largest deltas were G1 TTFT max +2.25 ms and G1 end-to-end p99 +1.95 ms (the tables: up to +0.93 and +1.38 ms).

### Re-run with vLLM's stream ending

After phantom's stream ending was changed to vLLM's (see "Streaming"), the audit was run once more, in release, on the same machine, with the same command. On chat, GuideLLM asks for `"stream_options": {"include_usage": true, "continuous_usage_stats": true}` ([`request_handlers.py` L1174–L1177](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/backends/openai/request_handlers.py#L1174-L1177)). So each stream now ended with the last token's chunk, carrying `finish_reason`, then a usage chunk with `"choices": []`. Before, it ended with a chunk with an empty `delta`, `finish_reason` and `usage`. In GuideLLM's source, neither final chunk is a token line, since neither has `delta.content`, and GuideLLM keeps the last `usage` it sees ([L1301–L1325](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/backends/openai/request_handlers.py#L1301-L1325)). The change should therefore not move GuideLLM's measurements. phantom sends no per-chunk usage, which `continuous_usage_stats` asks for; GuideLLM would keep only the last one.

- **Exit and tokens.** All 4 scenarios exited 0. GuideLLM's output token count was 32 (mean, min and max) in every scenario, and so was phantom's per completed line.
- **Request counts.** They matched in G2, G3 and G4 (147/4, 38/1, 147/4). In G1, GuideLLM reported 147 successful and 3 incomplete, and phantom logged 147 completed lines and 4 `"completed": false` lines. In the previous re-run, the mismatch was in G2.
- **Against the previous re-run** (one run each):
  - **Send lateness** p99 was 5.0 ms in G1 (previously 16.3 ms), 3.5 ms in G2 (4.7 ms) and 4.1 ms in G4 (4.8 ms).
  - **TTFT − intended truth** at p99 was −3.6 ms in G1 (−14.3 ms), −1.5 ms in G2 (−2.5 ms) and −2.6 ms in G4 (−3.9 ms).
  - **TTFT − server truth** was +0.42 to +0.80 ms from p50 to p99 in G1, G2 and G4, and +1.33 ms at G2's maximum, a stalled request (previously +0.35 to +1.35 ms, and +2.25 ms at G1's maximum). In G3, with 38 requests, p99 is the maximum: +7.85 ms (+0.51 ms).
  - **Per-request ITL − truth** was −0.007 to +0.008 ms (−0.017 to +0.004 ms).
  - **End-to-end − server truth** was +0.62 to +0.93 ms (+0.63 to +1.95 ms).
  - **Scheduled end-to-end − intended truth** was +1.46 to +2.58 ms (+1.92 to +3.00 ms).

Candidate explanations, none of them verified:

- **What each side's clock covers.** GuideLLM's TTFT and `request_latency` start at its header write and end when its client has read the line. phantom's truth starts at handler entry and ends at the chunk hand-off. GuideLLM's side also includes httpx writing the request, the loopback trip both ways, hyper's write, and the client reading and parsing each SSE line. That would put a small positive delta on TTFT and end-to-end, as seen.
- **Actual send against intended send.** GuideLLM's TTFT is measured from its actual header write, and intended TTFT from the intended send time. Send lateness is in the second but not the first, which would explain the negative TTFT deltas against intended truth, largest where lateness is largest (p99). GuideLLM reports no scheduled TTFT, so there is no like-for-like column for it.
- **The intended origin.** The origin carries the smallest send-to-handler delay of the run, and GuideLLM's `targeted_start` is its own schedule on its own clock. The two schedules can differ by a fixed offset, which would appear as a constant in the scheduled end-to-end delta (+1.4 to +2.5 ms).
- **Send lateness of about 20–30 ms for a few requests.** GuideLLM ran 10 worker processes for the constant profile (`worker_count` 10 in its JSON). Opening a new connection, or process scheduling, could make a few sends late. This audit did not identify which requests were late or why.
- **Lower send lateness without `validate_backend: false`.** Each GuideLLM worker process calls its backend's `validate()` at startup, before the workers synchronise ([`worker.py` L284–L289](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/scheduler/worker.py#L284-L289)), and with validation on, that is a `GET /health` through the worker's HTTP client. If that leaves a connection open, a worker's first timed request no longer opens one. This would lower the lateness of the few late sends, and with it the TTFT and scheduled end-to-end differences against intended truth. It was not checked, and one run is one sample.
- **One request count off by one in each re-run (G2, then G1).** GuideLLM counted 150 requests and phantom logged 151. One request reached phantom's handler and was abandoned, but is not in GuideLLM's totals. A request started at the 30 s deadline and dropped by GuideLLM's measurement window is a candidate. It moved between scenarios, so it is not tied to one profile. The runs did not record which request it was.
- **G3's TTFT maximum in the second re-run (+7.85 ms).** A single request whose first token GuideLLM recorded about 7.9 ms after phantom's hand-off. As for G1's end-to-end maximum below, event-loop or garbage-collector pauses and wall-clock steps are candidates. The change of stream ending comes after the first token, so it is not a candidate here.
- **One late end-to-end maximum (G1, +9.21 ms).** A single request whose end GuideLLM recorded about 9 ms after phantom's `[DONE]` hand-off. Python event-loop or garbage-collector pauses in a worker are candidates. So is a step in the wall clock, since GuideLLM's timestamps are `time.time()`, not a monotonic clock.

Limits:

- **Loopback only.** GuideLLM and phantom share one machine and one kernel. Nothing here measures a real network.
- **One chunk = one token.** phantom sends one `" tok"` per chunk and reports one completion token per chunk. A real server may put several tokens in one chunk, and GuideLLM's per-request ITL would then divide by tokens, not chunks.
- **GuideLLM's token counting.** GuideLLM takes the output token count from the server's `usage`, falling back to the number of token-bearing lines. gpt2 encodes `" tok"` as 2 tokens, so a count from GuideLLM's tokenizer would be twice phantom's; the tokenizer only sized prompts here.
- **The tables come from runs with the two workarounds and phantom's earlier stream ending.** Each re-run is one run, compared with the 3-run ranges or with the previous re-run.
- **Truth is server-side plus intended-schedule.** Server truth stops at phantom's chunk hand-off: it cannot see the response being written, carried or read. Intended truth also depends on the origin formula and on seq order (below).
- **Intended truth for constant profiles only.** A synchronous run has no schedule, so G3 is compared with G2's intended truth, from a different run.
- **Request order is assumed.** GuideLLM sends no request id. Request `i` is matched to phantom's `seq` `i`, assuming sends arrive in schedule order. At 5 requests/s, sends are 200 ms apart and no request was later than one period, so a reordering would need a send more than 200 ms late.
- **Poisson is not audited.** Its intended send times are random draws inside GuideLLM, unknown to phantom, so there is no intended-schedule truth to compare with.
- **Release build, one machine, 3 runs.**

## Audit: vLLM bench serve

`tests/audit_vllm.rs` runs `vllm bench serve` from [vLLM](https://github.com/vllm-project/vllm) 0.32.0 against an in-process phantom that streams. It compares vLLM's time to first token (TTFT), time per output token (TPOT), inter-token latency (ITL) and end-to-end latency (E2EL) with phantom's per-token ground truth. This is a report, not a pass/fail gate for vLLM. The only assertions are that vLLM exits 0, its result JSON parses, and phantom logs at least 20 requests.

vLLM is an external Python tool. It is installed in a virtual environment outside the repo, and the repo has no Python files or Python dependencies:

```sh
cd ~/audit-tools && uv venv vllm-venv && uv pip install --python vllm-venv vllm   # installed vLLM 0.32.0
```

Run the audit with:

```sh
VLLM_BIN=~/audit-tools/vllm-venv/bin/vllm \
    cargo test --release -p phantom --test audit_vllm -- --ignored --nocapture --test-threads=1
```

Without `VLLM_BIN`, or if it does not name a file, the scenarios fail with a message that says so. The test also has four tests that run without vLLM: the `VLLM_BIN` check, the removal of lines without a `client_id` on a hand-written log, reading Python's `Infinity` in vLLM's JSON, and the replica of numpy's percentile on values computed by numpy itself.

phantom: `--service-ms 1 --ttft-ms 100 --itl-ms 20 --output-tokens 64 --token-text " the"`, seed 1, a fresh phantom for each scenario. Each scenario runs vLLM in a temporary directory:

```sh
vllm bench serve --backend openai --base-url http://127.0.0.1:<port> --endpoint /v1/completions \
  --model phantom --tokenizer gpt2 --dataset-name random --random-input-len 16 --random-output-len 32 \
  --ignore-eos --num-prompts 150 --request-rate <rate> [--burstiness <b>] [--max-concurrency 4] --seed 42 \
  --request-id-prefix '' --percentile-metrics ttft,tpot,itl,e2el --metric-percentiles 50,90,99,100 \
  --save-result --save-detailed --result-dir <tmp> --result-filename vllm.json --disable-tqdm
```

| scenario | vLLM | phantom | intended-schedule truth |
|:---|:---|:---|:---|
| V1 | `--request-rate 5 --burstiness inf` (constant spacing) | no stall | yes |
| V2 | `--request-rate 5 --burstiness inf` | `--stall 10000:2000` | yes |
| V3 | `--request-rate 5 --burstiness 1` (Poisson) | `--stall 10000:2000` | no: server truth only |
| V4 | `--request-rate inf --max-concurrency 4` (closed loop) | `--stall 10000:2000` | no: server truth only |

Notes on the command:

- **Flags.** All are from `vllm bench serve --help` (0.32.0). The default `--backend openai` posts to `/v1/completions`, which has no role chunk. `--metric-percentiles` takes 100 as well; numpy's 100th percentile is the maximum, so `p100` is vLLM's maximum.
- **`--request-id-prefix ''`.** vLLM sends request `i`'s id as the `x-request-id` header ([`endpoint_request_func.py` L139–L146](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L139-L146)), and the random dataset builds it as prefix + `i` ([`datasets.py` L654–L661](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/datasets/datasets.py#L654-L661)). With an empty prefix the id is `i`, which phantom logs as `client_id`.
- **The token text and the tokenizer.** gpt2 encodes `" the"` as 1 token (id 262), and 32 × `" the"` as 32 tokens. phantom's default `" tok"` is 2 tokens (ids 284 and 74), and 32 × `" tok"` is 64. vLLM re-tokenizes the text only when it reads no `usage` (see (d)). With phantom's current stream ending it reads phantom's `usage`, so the token text does not set the count; `" the"` is kept so that a re-tokenized count would also be 32. Two check runs with the default `" tok"` (2 requests each) show the difference. With phantom's earlier ending, vLLM reported 64 output tokens per request for 32 chunks, and TPOT p99 of 9.85 ms for a 20 ms ITL. With the current ending, it reported 32, and TPOT p99 of 20.00 ms.
- **vLLM's result JSON** is written by Python's `json` module, which writes an infinite `--request-rate` or `--burstiness` as `Infinity`. JSON has no such literal, so the test reads it as a string before parsing.

### What vLLM bench serve measures

Read from the installed source. `serve.py`, `endpoint_request_func.py` and `datasets.py` are byte-identical to the same files at the `v0.32.0` tag, and numpy's `_function_base_impl.py` to numpy's `v2.4.6` tag, so the links match the quoted lines. Timestamps are `time.perf_counter()`, a monotonic clock. Results are in seconds and reported in ms (× 1000).

**(a) TTFT** runs from just before the request is posted to the first SSE message whose `choices` is non-empty, whatever its text ([`endpoint_request_func.py` L227–L232](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L227-L232), [L255–L266](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L255-L266)):

```python
    st = time.perf_counter()
    output.start_time = st
    most_recent_timestamp = st
    try:
        async with session.post(url=api_url, json=payload, headers=headers) as response:
```

```python
                            if choices := data.get("choices"):
                                # Note that text could be empty here
                                # e.g. for special tokens
                                text = choices[0].get("text")
                                timestamp = time.perf_counter()
                                # First token
                                if not first_chunk_received:
                                    first_chunk_received = True
                                    output.ttft = timestamp - st
```

`st` is taken inside the concurrency semaphore (see (f)), so time spent waiting for it is not in TTFT. The chat backend (`openai-chat`) has the same rule, so there a role chunk would be TTFT ([L429–L438](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L429-L438)).

**(b) ITL is per chunk.** Every later message with non-empty `choices` adds the gap since the previous one ([`endpoint_request_func.py` L268–L273](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L268-L273)), and the gaps of all successful requests are pooled into one list ([`serve.py` L632](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L632)):

```python
                                # Decoding phase
                                else:
                                    output.itl.append(timestamp - most_recent_timestamp)

                                most_recent_timestamp = timestamp
                                generated_text += text or ""
```

```python
            itls += outputs[i].itl
```

The request's latency (E2EL) is `most_recent_timestamp - st`: it ends at the last message with non-empty `choices`, not at `[DONE]` ([L287](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L287)). phantom's last message with `choices` is its last token's chunk, which carries `finish_reason` (see "Streaming"), so each request has 31 ITL entries, one per token gap. phantom's earlier ending sent one more chunk with `choices` after the last token (`text` `""`, `finish_reason` `"length"`, and `usage`), and vLLM took it as one more message: 32 ITL entries per request, the last being the gap from token 31 to that chunk.

**(c) TPOT is per request:** `(latency − TTFT) / (output tokens − 1)`, one value per request with more than one output token ([`serve.py` L625–L629](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L625-L629)):

```python
            tpot = 0.0
            if output_len > 1:
                latency_minus_ttft = outputs[i].latency - outputs[i].ttft
                tpot = latency_minus_ttft / (output_len - 1)
                tpots.append(tpot)
```

**(d) Output tokens.** The server's `usage.completion_tokens` is read only from a message without `choices` ([`endpoint_request_func.py` L274–L277](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L274-L277)). If none was read, vLLM re-tokenizes the generated text with `--tokenizer` ([`serve.py` L609–L622](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L609-L622)):

```python
                            elif usage := data.get("usage"):
                                output.output_tokens = usage.get("completion_tokens")
                                if (pt := usage.get("prompt_tokens")) is not None:
                                    output.prompt_len = pt
```

```python
            if not output_len:
                if tokenizer is None:
                    output_len = 1
                else:
                    # We use the tokenizer to count the number of output tokens
                    # for some serving backends instead of looking at
                    # len(outputs[i].itl) since multiple output tokens may be
                    # bundled together
                    # Note : this may inflate the output token count slightly
                    output_len = len(
                        tokenizer(
                            outputs[i].generated_text, add_special_tokens=False
                        ).input_ids
                    )
```

vLLM's completions request asks for the usage chunk with `"stream_options": {"include_usage": true}` ([`endpoint_request_func.py` L214–L217](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L214-L217)). phantom then ends each stream with a chunk that has `"choices": []` and `usage`, as vLLM's server does, and vLLM reads `completion_tokens` from it: 32. phantom's earlier ending put `usage` on a chunk that also had `choices`, so vLLM never read it and re-tokenized the text instead; 32 × `" the"` is also 32 gpt2 tokens.

**(e) Send times.** Each request's delay is drawn before sending. At an infinite rate it is 0. At an infinite burstiness it is exactly `1 / rate`. Otherwise it is a gamma draw with shape `burstiness` and mean `1 / rate` (exponential at burstiness 1) ([`serve.py` L468–L479](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L468-L479)):

```python
            if current_request_rate == float("inf"):
                delay_ts.append(0)
            elif burstiness == float("inf"):
                # when burstiness tends to infinity, the delay time becomes constant
                # and tends to the inverse of the request rate
                delay_ts.append(1.0 / current_request_rate)
            else:
                theta = 1.0 / (current_request_rate * burstiness)

                # Sample the request interval from the gamma distribution.
                # If burstiness is 1, it follows exponential distribution.
                delay_ts.append(np.random.gamma(shape=burstiness, scale=theta))
```

The delays are summed into send times and, with no ramp-up, rescaled so the last send is at `num_prompts / rate` ([L481–L497](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L481-L497)). With `--burstiness inf` every delay is `1 / rate`, so the rescale factor is 1 (up to float rounding) and request `i` is due at `start + (i + 1) / rate`: constant spacing. A large finite burstiness is only near-constant: the gamma draw's coefficient of variation is `1 / sqrt(burstiness)`. The sends sleep until `start_ts + delay` on `time.time()`, a wall clock ([L509–L516](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L509-L516)):

```python
    start_ts = time.time()
    for request_index, request in enumerate(input_requests):
        if delay_ts[request_index] > 0:
            current_ts = time.time()
            sleep_interval_s = start_ts + delay_ts[request_index] - current_ts
            if sleep_interval_s > 0:
                await asyncio.sleep(sleep_interval_s)
        yield request, request_rates[request_index]
```

**(f) `--max-concurrency`** does not change when requests are issued, only when they run. Each issued request waits on a semaphore of that size before its request function, and so before `st` ([`serve.py` L975–L985](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L975-L985)). The time spent waiting is recorded separately as `client_queue_time` and is not in TTFT, ITL, TPOT or E2EL. The connection pool is limited to the same number ([L826–L828](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L826-L828)). With `--request-rate inf`, all 150 requests are issued at once and run 4 at a time: a closed loop.

```python
    async def limited_request_func(
        request_func_input, session, pbar, request_arrival_time
    ):
        async with semaphore:
            output = await request_func(
                request_func_input=request_func_input, session=session, pbar=pbar
            )
        # Preserve start_time as the time the request was sent. It is used by
        # throughput calculations; record client-side semaphore delay separately.
        output.client_queue_time = output.start_time - request_arrival_time
        return output
```

**(g) No initial test request and no warm-up by default.** The initial single-prompt test is sent only as the ready check, which runs only if `--ready-check-timeout-sec` is above 0 ([`serve.py` L876–L892](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L876-L892)). That flag defaults to 0 ([L1941–L1946](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L1941-L1946)), and `--num-warmups` defaults to 0 ([L1733–L1737](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L1733-L1737)):

```python
    if ready_check_timeout_sec > 0:
        test_output = await wait_for_endpoint(
            request_func,
            test_input,
            session,
            timeout_seconds=ready_check_timeout_sec,
        )
        if not test_output.success:
            raise ValueError(
                "Initial test run failed - Please make sure benchmark "
                "arguments are correctly specified. "
                f"Error: {test_output.error}"
            )
        else:
            print("Initial test run completed.")
    else:
        print("Skipping endpoint ready check.")
```

The test and warm-up requests are built without a `request_id` ([L861–L874](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L861-L874)), so they carry no `x-request-id`. That is how the test identifies them: a phantom line without a `client_id` is not a benchmark request, and is taken out before any truth is computed. In every run there were none. In V1, V2 and V3, every line's `client_id` equalled its `seq`. In V4 it did on 101–125 of 150 lines: 4 requests run at once and reach phantom in varying order. V4 uses no seq-keyed truth. vLLM also requests `GET /metrics`, for speculative-decoding and diffusion metrics, before and after the run ([L196–L207](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L196-L207), [L269–L281](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L269-L281)). phantom answers 404 and logs nothing, and vLLM then skips those metrics.

**(h) Percentiles** are numpy's `np.percentile` with its default method, `linear`: the value at virtual index `(n − 1) × p / 100`, interpolated between its two neighbours ([`serve.py` L757–L759](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L757-L759), likewise for TPOT, ITL and E2EL; numpy's [`_function_base_impl.py` L131–L134](https://github.com/numpy/numpy/blob/v2.4.6/numpy/lib/_function_base_impl.py#L131-L134) and [`_lerp`, L4596–L4599](https://github.com/numpy/numpy/blob/v2.4.6/numpy/lib/_function_base_impl.py#L4596-L4599)):

```python
        percentiles_ttft_ms=[
            (p, np.percentile(ttfts or 0, p) * 1000) for p in selected_percentiles
        ],
```

```python
    'linear': {
        'get_virtual_index': lambda n, quantiles: (n - 1) * quantiles,
        'fix_gamma': lambda gamma, _: gamma,
    },
```

`common::numpy_percentile` replicates it, including `_lerp` interpolating down from the upper neighbour when the weight is at least 0.5, and matches numpy's values exactly on the test's fixtures. Unlike nearest rank, it returns a value between two samples: in a stalled run, p99 can fall between a normal request and a stalled one.

### Truth

Truth comes from phantom's log through `tests/common/mod.rs` (`stream_lines`, `take_untagged`, `stream_schedule_truth`). Only completed lines with a `client_id` are used.

- **Server TTFT** = `token_ns[0] − arrival_ns`. **Server end-to-end** = `sent_ns − arrival_ns`.
- **Per-gap ITL truth**: every consecutive difference of `token_ns`, pooled over requests (31 per request). Compared with vLLM's ITL.
- **Per-request mean ITL truth**: `(token_ns[last] − token_ns[0]) / (tokens − 1)`, one value per request: the gaps after the first token. Compared with vLLM's TPOT.
- **Intended-schedule truth**, V1 and V2 only. Request `i` is phantom's `seq` `i`, and its intended send time is `origin + i × 200 ms`, with the same origin formula as the other audits (`schedule_origin`: the least-lagged request). Intended TTFT = `token_ns[0] − intended`, intended end-to-end = `sent_ns − intended`, and send lateness = `arrival_ns − intended`. vLLM reports no corrected or scheduled latency, so its TTFT and E2EL are shown against this truth as well.
- **V3** sends at Poisson times drawn inside vLLM, which phantom does not know, and **V4** is a closed loop with no schedule, so both have server truth only.

Each truth is shown at nearest rank (NR) and at numpy's method (np), the method vLLM uses.

### Results

3 runs, release build, with phantom's current stream ending (the last token's chunk carries `finish_reason`, then a usage chunk with `"choices": []`). Intel(R) Core(TM) i7-14650HX, kernel 6.6.87.2-microsoft-standard-WSL2, rustc 1.94.1 (e408947bf 2026-03-25), tokio 1.53.3, release build (opt-level 3), on AC power, Windows Balanced power plan; Python 3.13.15, vLLM 0.32.0, aiohttp 3.14.5, numpy 2.4.6, transformers 5.19.0. Every cell is the min to max over the 3 runs, in ms. Deltas are vLLM − truth, at nearest rank (NR) and at numpy's method (np). The p100 row is the maximum.

**TTFT** (ms)

| scenario | pct | vLLM | server truth | − server (NR) | − server (np) | intended truth | − intended (NR) | − intended (np) |
|:---|:---|---:|---:|---:|---:|---:|---:|---:|
| V1 | p50 | 102.023–102.207 | 100.992–101.089 | +1.031 to +1.119 | +1.027 to +1.118 | 101.782–101.875 | +0.167 to +0.351 | +0.164 to +0.349 |
|  | p90 | 102.787–102.879 | 101.651–101.769 | +1.019 to +1.227 | +1.018 to +1.226 | 102.914–103.152 | −0.364 to −0.035 | −0.365 to −0.035 |
|  | p99 | 103.212–103.656 | 101.951–102.104 | +1.108 to +1.706 | +1.116 to +1.711 | 103.634–104.073 | −0.861 to −0.237 | −0.718 to −0.203 |
|  | p100 | 103.453–103.907 | 102.129–102.209 | +1.294 to +1.698 | +1.294 to +1.698 | 103.690–105.209 | −1.302 to −0.236 | −1.302 to −0.236 |
| V2 | p50 | 102.131–102.425 | 101.054–101.097 | +1.077 to +1.328 | +1.076 to +1.326 | 101.741–102.155 | +0.270 to +0.467 | +0.241 to +0.466 |
|  | p90 | 102.932–103.229 | 101.671–101.920 | +1.196 to +1.420 | +1.194 to +1.419 | 103.321–103.689 | −0.460 to −0.205 | −0.469 to −0.211 |
|  | p99 | 1703.195–1706.447 | 1799.842–1800.224 | −96.648 to −93.485 | +1.369 to +4.384 | 1801.041–1802.143 | −97.847 to −95.696 | +0.214 to +2.390 |
|  | p100 | 2001.894–2005.159 | 2000.592–2001.553 | +1.302 to +3.606 | +1.302 to +3.606 | 2001.026–2002.106 | +0.868 to +3.053 | +0.868 to +3.053 |
| V3 | p50 | 102.114–102.202 | 101.099–101.174 | +0.992 to +1.055 | +0.979 to +1.055 | — | — | — |
|  | p90 | 102.731–103.101 | 101.679–101.800 | +1.020 to +1.301 | +1.019 to +1.301 | — | — | — |
|  | p99 | 1010.957–1012.579 | 1249.391–1250.539 | −238.542 to −237.960 | +0.945 to +1.358 | — | — | — |
|  | p100 | 1858.175–1859.346 | 1857.157–1858.314 | +1.017 to +1.052 | +1.017 to +1.052 | — | — | — |
| V4 | p50 | 102.556–102.650 | 101.139–101.201 | +1.355 to +1.511 | +1.342 to +1.509 | — | — | — |
|  | p90 | 103.540–103.839 | 101.677–101.719 | +1.863 to +2.128 | +1.861 to +2.127 | — | — | — |
|  | p99 | 104.367–104.679 | 101.931–102.003 | +2.377 to +2.675 | +2.384 to +2.684 | — | — | — |
|  | p100 | 104.694–105.290 | 101.942–102.081 | +2.616 to +3.349 | +2.616 to +3.349 | — | — | — |

**TPOT** (ms), against the per-request mean gap

| scenario | pct | vLLM | per-request mean truth | − (NR) | − (np) |
|:---|:---|---:|---:|---:|---:|
| V1 | p50 | 20.003–20.005 | 20.001–20.002 | +0.001 to +0.003 | +0.001 to +0.003 |
|  | p90 | 20.020–20.023 | 20.019–20.021 | +0.001 to +0.003 | +0.001 to +0.002 |
|  | p99 | 20.028–20.033 | 20.028–20.031 | −0.003 to +0.004 | −0.001 to +0.004 |
|  | p100 | 20.033–20.036 | 20.030–20.035 | +0.001 to +0.003 | +0.001 to +0.003 |
| V2 | p50 | 19.998–20.006 | 20.000–20.006 | −0.001 to +0.001 | −0.001 to +0.001 |
|  | p90 | 20.020–20.033 | 20.020–20.029 | 0.000 to +0.004 | 0.000 to +0.004 |
|  | p99 | 84.170–84.204 | 84.446–84.502 | −0.297 to −0.277 | +0.005 to +0.009 |
|  | p100 | 84.517–84.523 | 84.510–84.520 | +0.001 to +0.013 | +0.001 to +0.013 |
| V3 | p50 | 20.000–20.003 | 19.998–20.001 | +0.002 to +0.003 | +0.002 |
|  | p90 | 20.016–20.022 | 20.014–20.019 | +0.002 to +0.004 | +0.002 to +0.003 |
|  | p99 | 20.030–20.035 | 20.029–20.030 | +0.001 to +0.006 | +0.002 to +0.006 |
|  | p100 | 83.904–84.487 | 83.899–84.486 | −0.002 to +0.005 | −0.002 to +0.005 |
| V4 | p50 | 20.000–20.001 | 19.997–19.998 | +0.003 to +0.004 | +0.003 to +0.004 |
|  | p90 | 20.020–20.024 | 20.016–20.019 | +0.003 to +0.007 | +0.003 to +0.006 |
|  | p99 | 84.040–84.502 | 84.032–84.496 | −0.011 to +0.008 | +0.006 to +0.008 |
|  | p100 | 84.109–84.505 | 84.102–84.500 | +0.003 to +0.007 | +0.003 to +0.007 |

**ITL** (ms), against every gap

| scenario | pct | vLLM | per-gap truth | − (NR) | − (np) |
|:---|:---|---:|---:|---:|---:|
| V1 | p50 | 20.254–20.264 | 20.268–20.282 | −0.018 to −0.013 | −0.018 to −0.013 |
|  | p90 | 20.489–20.531 | 20.467–20.516 | +0.015 to +0.022 | +0.015 to +0.022 |
|  | p99 | 20.709–20.799 | 20.665–20.755 | +0.036 to +0.045 | +0.037 to +0.045 |
|  | p100 | 21.130–21.987 | 21.041–21.898 | +0.039 to +0.695 | +0.039 to +0.695 |
| V2 | p50 | 20.238–20.277 | 20.262–20.309 | −0.032 to −0.024 | −0.032 to −0.025 |
|  | p90 | 20.516–20.565 | 20.486–20.548 | +0.017 to +0.030 | +0.017 to +0.030 |
|  | p99 | 20.749–20.840 | 20.736–20.776 | +0.014 to +0.064 | +0.014 to +0.064 |
|  | p100 | 2019.109–2021.468 | 2019.034–2019.389 | +0.066 to +2.434 | +0.066 to +2.434 |
| V3 | p50 | 19.894–19.970 | 19.892–19.965 | +0.003 to +0.005 | +0.002 to +0.005 |
|  | p90 | 20.627–20.655 | 20.614–20.643 | +0.009 to +0.013 | +0.008 to +0.013 |
|  | p99 | 20.918–20.967 | 20.896–20.937 | +0.022 to +0.034 | +0.022 to +0.035 |
|  | p100 | 2000.740–2019.562 | 2000.867–2019.617 | −0.127 to −0.040 | −0.127 to −0.040 |
| V4 | p50 | 20.206–20.285 | 20.231–20.299 | −0.025 to −0.015 | −0.025 to −0.015 |
|  | p90 | 20.533–20.590 | 20.517–20.575 | +0.013 to +0.016 | +0.013 to +0.016 |
|  | p99 | 20.749–20.879 | 20.697–20.830 | +0.049 to +0.067 | +0.050 to +0.067 |
|  | p100 | 2007.454–2019.521 | 2007.430–2019.501 | −0.017 to +0.023 | −0.017 to +0.023 |

**End-to-end** (ms; vLLM E2EL)

| scenario | pct | vLLM | server truth | − server (NR) | − server (np) | intended truth | − intended (NR) | − intended (np) |
|:---|:---|---:|---:|---:|---:|---:|---:|---:|
| V1 | p50 | 722.213–722.266 | 721.078–721.186 | +1.081 to +1.145 | +1.076 to +1.145 | 721.809–722.012 | +0.201 to +0.414 | +0.200 to +0.413 |
|  | p90 | 722.832–722.949 | 721.664–721.687 | +1.168 to +1.262 | +1.167 to +1.261 | 722.916–723.130 | −0.299 to +0.002 | −0.303 to −0.004 |
|  | p99 | 723.248–723.781 | 721.957–722.055 | +1.193 to +1.824 | +1.261 to +1.840 | 723.972–724.742 | −1.108 to −0.551 | −1.060 to −0.318 |
|  | p100 | 723.461–724.388 | 721.960–722.111 | +1.427 to +2.429 | +1.427 to +2.429 | 724.001–724.855 | −1.190 to −0.450 | −1.190 to −0.450 |
| V2 | p50 | 722.312–722.411 | 721.128–721.220 | +1.113 to +1.283 | +1.112 to +1.281 | 721.886–722.184 | +0.227 to +0.465 | +0.226 to +0.454 |
|  | p90 | 723.224–723.458 | 721.842–721.981 | +1.317 to +1.615 | +1.313 to +1.613 | 723.583–724.102 | −0.644 to −0.359 | −0.664 to −0.367 |
|  | p99 | 2712.094–2712.410 | 2719.623–2720.383 | −8.235 to −7.530 | +1.171 to +1.396 | 2721.323–2722.166 | −10.073 to −8.993 | −0.271 to +0.591 |
|  | p100 | 2721.816–2722.284 | 2720.294–2720.893 | +1.152 to +1.830 | +1.152 to +1.830 | 2721.546–2722.265 | −0.141 to +0.737 | −0.141 to +0.737 |
| V3 | p50 | 722.075–722.323 | 721.081–721.204 | +0.993 to +1.171 | +0.991 to +1.170 | — | — | — |
|  | p90 | 722.803–723.152 | 721.778–721.820 | +0.983 to +1.374 | +0.982 to +1.374 | — | — | — |
|  | p99 | 2180.246–2181.243 | 2476.908–2477.891 | −296.662 to −296.448 | +1.145 to +1.313 | — | — | — |
|  | p100 | 2702.503–2721.723 | 2701.201–2720.468 | +1.255 to +1.371 | +1.255 to +1.371 | — | — | — |
| V4 | p50 | 722.568–722.739 | 721.081–721.165 | +1.467 to +1.574 | +1.461 to +1.574 | — | — | — |
|  | p90 | 723.713–724.160 | 721.676–721.782 | +1.930 to +2.454 | +1.929 to +2.452 | — | — | — |
|  | p99 | 2708.253–2722.396 | 2706.662–2720.073 | +0.638 to +2.324 | +0.922 to +2.333 | — | — | — |
|  | p100 | 2709.751–2722.756 | 2708.422–2720.116 | +1.046 to +2.640 | +1.046 to +2.640 | — | — | — |

**Requests, ITL samples and send lateness**

| scenario | vLLM completed / failed | phantom completed / `completed: false` / no client_id | vLLM ITL samples | phantom gaps | lateness p50 ms | lateness p99 ms | lateness max ms |
|:---|---:|---:|---:|---:|---:|---:|---:|
| V1 | 150/0 | 150/0/0 | 4650 | 4650 | 0.731–0.752 | 2.425–2.914 | 2.611–3.350 |
| V2 | 150/0 | 150/0/0 | 4650 | 4650 | 0.669–1.053 | 2.300–3.273 | 2.350–4.983 |
| V3 | 150/0 | 150/0/0 | 4650 | 4650 | — | — | — |
| V4 | 150/0 | 150/0/0 | 4650 | 4650 | — | — | — |

These ranges are the observed min–max over the 3 runs, not bounds: with only 3 runs a new run falls outside the range about half the time by chance, and this audit sets no pass criteria at all — it asserts only that vLLM exited 0, its result JSON parsed, and phantom logged at least 20 requests.

In every run, vLLM completed all 150 requests with none failed, and phantom logged 150 completed lines with none abandoned. Every response had 32 tokens on both sides, and vLLM's count came from phantom's `usage`. vLLM's ITL list had 31 entries per request (4650), the same as phantom's gaps. In V1 and V2, no request arrived more than one period (200 ms) after its intended send time.

What the tables show:

- **TTFT against server truth.** At numpy's method, vLLM − server TTFT was +0.98 to +1.71 ms in V1 and V3, +1.08 to +4.38 ms in V2, and +1.34 to +3.35 ms in V4. The V2 values above +1.5 ms are all at p99 and p100, in runs 1 and 3 (+2.49 to +4.38 ms). At nearest rank the deltas were in the same ranges, except at p99 in V2 (−93.5 to −96.6 ms) and V3 (−238.0 to −238.5 ms).
- **Percentile method in the stall tail.** At p99, numpy's virtual index `(n − 1) × 0.99` = 147.51 lies between the 148th and 149th smallest of 150 values, and nearest rank takes the 149th. In V2 and V3 both are requests the stall delayed, by different amounts. numpy returns the point 51% of the way from the first to the second, a value no request had. vLLM's TTFT p99 was 1703.2–1706.4 ms in V2 (nearest-rank truth 1799.8–1800.2 ms). Its TTFT and E2EL p99 in V3 were 1011.0–1012.6 and 2180.2–2181.2 ms (nearest-rank truth 1249.4–1250.5 and 2476.9–2477.9 ms). The same effect, smaller, shows in V2's E2EL p99 at nearest rank (−7.5 to −8.2 ms against server truth) and V2's TPOT p99 (−0.28 to −0.30 ms). Against truth taken at numpy's method, all of these cells are within the ranges of the other cells.
- **TTFT against intended truth (V1, V2).** vLLM − intended TTFT was −1.30 to +0.47 ms at both methods, except V2 p99 and p100 (+0.21 to +3.05 ms at numpy's method) and V2 p99 at nearest rank (−95.7 to −97.8 ms, the tail effect). Send lateness was 0.67–1.05 ms at p50, 2.30–3.27 ms at p99 and at most 4.98 ms.
- **TPOT.** vLLM's TPOT matched the per-request mean truth within −0.001 to +0.013 ms at numpy's method in every cell. A stalled request's TPOT was about 84 ms: a per-request mean spreads its 2 s gap over 31 gaps.
- **ITL.** vLLM's ITL matched per-gap truth within −0.127 to +0.067 ms at every percentile from p50 to p99, and at the maximum in V3 and V4. The exceptions are three maxima, one gap each: V1 +0.695 ms (run 3), and V2 +1.314 and +2.434 ms (runs 1 and 3). In V2, V3 and V4, the maximum ITL was the stall itself, 2.00–2.02 s. At p50, vLLM's ITL was 0.013–0.032 ms below truth in V1, V2 and V4, and 0.002–0.005 ms above it in V3.
- **End-to-end against server truth.** vLLM − server was +0.92 to +2.64 ms in every cell at numpy's method. At nearest rank, V3 p99 was −296.4 to −296.7 ms and V2 p99 −7.5 to −8.2 ms, by the tail effect; the other nearest-rank cells were +0.64 to +2.64 ms.
- **End-to-end against intended truth (V1, V2).** vLLM − intended was −1.19 to +0.74 ms at numpy's method. At nearest rank it was in the same range, except V2 p99 (−9.0 to −10.1 ms, the tail effect).
- **Percentile method elsewhere.** Outside the cells named above (V2 TTFT, E2EL and TPOT p99; V3 TTFT and E2EL p99), nearest rank and numpy's method gave truths at most 0.46 ms apart (V1 intended end-to-end p99). At p100 both are the maximum.

Against the earlier ending. An earlier set of 3 runs used phantom's earlier stream ending: after the last token, one more chunk with `choices` (`text` `""`, `finish_reason` `"length"`) and `usage`. In those runs:

- vLLM's ITL list had 32 entries per request (4800 against 4650 gaps). The extra entry, the gap from the last token to that chunk, was 0.005–0.006 ms at p50 and at most 0.201 ms.
- vLLM re-tokenized the text instead of reading `usage`. With `" the"` the count was the same, 32.
- vLLM's ITL p50 was further below truth: −0.020 to −0.041 ms in V1 and V2 (now −0.013 to −0.032), and −0.006 to −0.020 ms in V3 (now +0.002 to +0.005).
- The other cells had about the same ranges as now. TTFT and end-to-end against server truth were +0.92 to +3.29 ms at numpy's method, and TPOT was within −0.001 to +0.022 ms. The large V2 tail values of runs 1 and 3 above did not occur.

The two sets of runs were made a day apart, 3 runs each. The ITL count and the token source changed as the source read in (a) and (d) predicts. Of the 160 delta cells in the tables, 126 have min–max ranges that overlap between the two sets, and 31 more are apart by at most 0.11 ms. The other 3 are nearest-rank cells at V2 p99 (E2EL against server and intended truth, and TPOT), by 0.27–8.3 ms. There the two values around numpy's index were further apart in the current runs, so nearest rank and numpy's method differ more (the tail effect above).

Candidate explanations, none of them verified:

- **What each side's clock covers.** vLLM's TTFT and E2EL start just before aiohttp posts the request and end when its client has parsed the message. phantom's truth starts at handler entry and ends at the chunk hand-off. vLLM's side also includes aiohttp writing the request, the loopback trip both ways, hyper's write, and reading and parsing each SSE message in Python. That would put a positive delta on TTFT and end-to-end, as seen (about +1 ms; GuideLLM's was about +0.5 ms). V4 has 4 requests in flight on one event loop, which would add more.
- **TTFT and E2EL against intended truth.** vLLM's values run from its actual send, intended truth from the intended send. Intended truth therefore includes the send lateness (0.67–1.05 ms at p50, at most 4.98 ms), and vLLM's values include the client-side time in the first candidate (about 1 ms). The two partly cancel, which would put the deltas near 0, as seen.
- **ITL p50 slightly below truth.** vLLM's gap is between the times its client parsed two messages, and phantom's between two hand-offs. A message read late lengthens one gap and shortens the next. That widens vLLM's gap distribution, which raises the upper percentiles (as seen at p90 and p99) and can lower the middle. The earlier ending's near-zero extra gaps (3% of the samples) lowered p50 further. This was not examined.
- **V2's stall tail in runs 1 and 3.** When the stall window ends, phantom releases at once the held token of every stream in the window, so vLLM's one event loop reads many streams' messages in a burst. Messages read later in the burst get later timestamps. That would raise TTFT and the stall ITL for some stalled requests, which sit at p99 and p100. The earlier runs had the same burst without deltas this large, and it was not examined.
- **One large ITL maximum (V1, +0.695 ms).** A single gap vLLM recorded longer than any gap phantom handed off. A pause in vLLM's event loop, or in reading the socket, between two messages is a candidate.

Limits:

- **Loopback only.** vLLM and phantom share one machine and one kernel. Nothing here measures a real network.
- **One chunk = one token.** phantom sends one `" the"` per chunk and reports one completion token per chunk in `usage`, which vLLM reads. A real server may put several tokens in one chunk; vLLM's ITL would then be per chunk, not per token. The token text is one gpt2 token, so a re-tokenized count would match too.
- **Completions endpoint only.** On the chat backend, vLLM would also take phantom's role chunk as the first token and add one more ITL entry for it (a). Chat was not run.
- **Truth is server-side plus intended-schedule.** Server truth stops at phantom's chunk hand-off: it cannot see the response being written, carried or read. Intended truth depends on the origin formula and on seq order.
- **Intended truth for constant spacing only.** V3's Poisson send times are drawn inside vLLM from its seed, and phantom does not know them. V4 has no schedule. Both have server truth only.
- **Request order.** In V1 and V2, intended truth is keyed by seq, and every line's `client_id` (vLLM's request index) equalled its `seq` in every run, so seq order was vLLM's send order. In V4, with 4 requests in flight, it was 97–106 of 150 lines.
- **The earlier ending is summarized, not tabulated.** Its 3 runs are compared above in a few figures only.
- **Release build, one machine, 3 runs.**

### oha, GuideLLM and vLLM compared

Only what the three source reads above established. "Not read" means this repo's audits did not read that part of the tool, not that the tool lacks it. The oha audit was non-streaming, so oha's streaming metrics were not read.

| | oha 1.16.0 | GuideLLM 0.8.1 | vLLM bench serve 0.32.0 |
|:---|:---|:---|:---|
| TTFT | not read | from the request's header write to the first SSE line with non-empty content (`delta.content`, reasoning or a tool call); a chat role chunk with `content` `""` is not counted | from just before the request is posted to the first SSE message with non-empty `choices`, whatever its text; on chat, a role chunk counts |
| ITL | not read | per request: `(last token line − first token line) / (output tokens − 1)`; its distribution weights each request by `output tokens − 1` | every gap between consecutive messages with non-empty `choices`, pooled over requests; a message with `choices` counts even if its text is empty |
| TPOT | not read | not read | per request: `(latency − TTFT) / (output tokens − 1)`, unweighted |
| Output tokens | not applicable | asks for usage (`include_usage` and `continuous_usage_stats`); the last `usage.completion_tokens` seen, else the number of token-bearing lines | asks for usage (`include_usage`); the server's `usage.completion_tokens` from a message without `choices`, else the generated text re-tokenized with `--tokenizer` |
| End of a request | not read | the end of the stream | the last message with non-empty `choices`, not `[DONE]` |
| Corrected or scheduled latency | `--latency-correction`: "Correct latency to avoid coordinated omission problem. It's ignored if -q is not set." (`oha --help`) | `request_scheduled_latency`, constant profile only: request end − targeted start, `start + index / rate` | none: TTFT, TPOT, ITL and E2EL run from the actual send; the wait for `--max-concurrency` is recorded separately as `client_queue_time` |
| Percentile | sorted values at the truncated index `(p / 100 × n) as usize`, no interpolation | first value whose weighted running sum (numpy `cumsum`) is ≥ q (`searchsorted`, `side="left"`) | `np.percentile`, linear interpolation between the two samples around `(n − 1) × p / 100` |
| Clock | not read | `time.time()`, a wall clock | `time.perf_counter()` for request timings; `time.time()` for the send schedule |
