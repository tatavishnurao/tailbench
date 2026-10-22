# Truth log

The ground-truth log phantom writes, and the limits of what it records ([LIMITS](#limits)).

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

## Audit truth

Each audit computes its truth from this log; the formulas are in each report:

- [tailbench](audits/tailbench.md#audit-results)
- [oha](audits/oha.md)
- [GuideLLM](audits/guidellm.md#truth)
- [vLLM bench serve](audits/vllm.md#truth)

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
