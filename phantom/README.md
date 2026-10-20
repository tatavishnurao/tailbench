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
| `--output-tokens N` | tokens per streamed response, > 0 (default 32); a request's smaller `max_tokens` wins |
| `--role-chunk WHEN` | when a chat stream's role chunk is sent: `with-first-token` (default) or `at-arrival` |
| `--log PATH` | ground-truth log, created or truncated |

Exactly one of `--service-ms` or `--lognormal` is required. At most one of each `--ttft-*` and `--itl-*` pair may be given. Invalid specs exit non-zero with a message before anything is bound or written. SIGINT or SIGTERM shuts down gracefully: in-flight requests, including streams, are answered, then the log is flushed.

Endpoints:

- `POST /v1/chat/completions` returns a fixed non-streaming chat completion (`id` depends on seq; nothing else varies), or an event stream when the body has `"stream": true`.
- `POST /v1/completions` does the same with the completions shapes: `text_completion` objects with `choices[0].text`.
- `GET /healthz` returns 200 and is not logged.

## Streaming

```sh
cargo run -p phantom -- --service-ms 1 --ttft-ms 100 --itl-ms 20 --output-tokens 5 --log /tmp/gt.jsonl
curl -N -H 'content-type: application/json' -d '{"stream":true}' http://127.0.0.1:8080/v1/chat/completions
```

A body with `"stream": true` (the JSON boolean) gets `text/event-stream`, one `data: {...}` event per chunk, each followed by a blank line:

- **chat:** a `chat.completion.chunk` with `delta.role` `"assistant"` (the role chunk), then one chunk per token with `delta.content` `" tok"`, then a chunk with `finish_reason` `"length"` and `usage` (`prompt_tokens` 8, `completion_tokens` = tokens sent), then `data: [DONE]`.
- **completions:** one `text_completion` chunk per token with `choices[0].text` `" tok"`, the same final chunk (with `text` `""`) and usage, then `data: [DONE]`. There is no role chunk.

Only `stream` and `max_tokens` are read from the body; everything else is ignored. Without `"stream": true`, including when the body is not JSON, both endpoints take the non-streaming path and log exactly the old fields.

Tokens per response: `min(max_tokens, --output-tokens)` when `max_tokens` is a whole number, else `--output-tokens`. `max_tokens` 0 still gets one token: a stream always carries at least one.

Timing, on the server's own clock:

- The schedule is absolute. Token 0 is scheduled at `arrival + scripted_ttft_ns`, and token k at token k−1's *scheduled* time + `scripted_itl_ns` for that gap, never at its actual hand-off. phantom sleeps until each scheduled time, so timer overshoot on one token does not delay the next: it does not accumulate over a stream. Each token's scheduled time is logged in `scheduled_token_ns`, and `token_ns[k] >= scheduled_token_ns[k]` for every k.
- If a token's scheduled time falls inside a stall window, that token and every later scheduled time move later by `window end − scheduled time` (logged in `token_stall_wait_ns` for that token, 0 for the others). The gaps scripted after the stall are kept as they are. A token moved to a window's end is not moved again, even if another window starts there: each window is applied separately, as for non-streaming requests.
- The chat role chunk is sent according to `--role-chunk`. With `with-first-token` (default), phantom hands it off just before token 0, so a client that times the first chunk and one that times the first content chunk see the same TTFT. With `at-arrival`, phantom hands it off as soon as the request arrives. That is before TTFT, and also before any stall, so the two clients see different TTFTs. Either way, chat lines log the hand-off time in `role_chunk_ns`.
- `scripted_ttft_ns` is a pure function of (seed, seq), and each `scripted_itl_ns` of (seed, seq, token index). Both use splitmix64 and Box-Muller like the service time, with their own keys, so they never reuse the service-time draw or each other's. With no `--ttft-*`, TTFT follows the service-time spec but takes its own draws.

## Script

- **t0** is the arrival of the first non-healthz request, on the server's monotonic clock. All logged times are ns since t0.
- **seq** comes from an atomic counter at arrival: 0, 1, 2, ... with no gaps. Every seq gets exactly one log line, completed or abandoned (see "Abandoned requests").
- **scripted_service_ns** is a pure function of (seed, seq): splitmix64 feeds Box-Muller for lognormal; fixed is `ms * 1e6`.
- **stall_wait_ns** is `window_end - arrival_ns` if arrival falls in a stall window (start inclusive, end exclusive), else 0.
- A non-streaming request is held until `arrival + stall_wait_ns`, then sleeps `scripted_service_ns`, then gets its response. Streams follow the token schedule in "Streaming" above.

## Log format

The first line is the manifest; then one line per chat or completions request, in the order responses were finished (not seq order):

```json
{"type":"manifest","phantom_version":"0.1.0","seed":1,"service":{"kind":"fixed","ms":20},"stalls":[{"start_ms":2000,"len_ms":500}],"ttft":{"kind":"fixed","ms":20},"itl":{"kind":"fixed","ms":10},"output_tokens":32,"role_chunk":"with-first-token","listen":"127.0.0.1:8080","start_unix_ns":1792507757152789210,"times":"ns since t0, the arrival of the first non-healthz request, on the server's monotonic clock"}
{"type":"req","seq":2,"arrival_ns":2161907075,"scripted_service_ns":20000000,"stall_wait_ns":338092925,"sent_ns":2522484756,"client_id":2}
```

The manifest's `ttft` is the TTFT spec in effect: the service-time spec unless `--ttft-*` was given. Its `role_chunk` is the `--role-chunk` mode in effect, `with-first-token` or `at-arrival`.

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
| `output_tokens` | tokens the response was scripted to carry, `min(max_tokens, --output-tokens)`; on a completed line, the tokens sent |
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
- **The request body is read only for `stream` and `max_tokens`.** It is read (up to 16 MiB), and everything else in it is ignored: any prompt gets the same response.
- **Chunk hand-off time is not wire time.** `token_ns` and a stream's `sent_ns` are taken immediately before each chunk is handed to the response stream. hyper's write to the socket, the kernel and the network come after them.
- **One chunk is one "token", with fixed text.** Every token is the chunk `" tok"`. A client that re-tokenizes the text, rather than counting chunks, may count a different number of tokens.
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
- **Requests aborted at the deadline.** These are not in oha's percentiles. Before abandoned requests were logged, phantom logged exactly 1000 requests and the aborted ones left no line. Now an aborted request that reached phantom's handler leaves a `"completed": false` line, and truth is computed over completed lines only. In one release run after that change, phantom's `completed: false` count matched oha's "aborted due to deadline" count only in O4 (1 = 1) and O5 (0 = 0). In O1 it was 1 against 2, and in O2 and O3 it was 0 against 1. So some requests oha counts as aborted at the deadline never reach phantom's handler: either they were not sent, or the connection closed before the headers were parsed.

Limits:

- **The reference is intended-schedule truth from tailbench's own run,** not an independent measurement of oha's run. oha sends no per-request id. Per-run truth matches its requests to log lines by arrival order (`seq`), assuming oha sends in schedule order and arrival order equals send order. Inside a post-stall burst, requests that arrive together may be permuted. That changes individual truth values, but by no more than the burst's spread.
- **Loopback only:** oha, tailbench and phantom share one machine and one kernel.
- **Corrected latency is oha's own definition.** `oha --help` (1.16.0) describes it as: "Correct latency to avoid coordinated omission problem. It's ignored if -q is not set."
- **Release build, one machine.** These numbers come from one machine; other machines and debug builds were not measured.
- **Different percentile conventions.** oha truncates the index and nearest rank rounds it up (see above). Comparing at nearest rank alone would show one rank of difference as a measurement difference.
