# Endpoints, streaming and CLI flags

How to start phantom, its flags, the endpoints it serves and the event stream it sends. The script behind its timings is in [spec.md](spec.md), and its log in [truth-log.md](truth-log.md).

## Usage

```sh
cargo run -p tailbench-phantom -- --listen 127.0.0.1:8080 --seed 1 --service-ms 20 --stall 2000:500 --log /tmp/gt.jsonl
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
cargo run -p tailbench-phantom -- --service-ms 1 --ttft-ms 100 --itl-ms 20 --output-tokens 5 --log /tmp/gt.jsonl
curl -N -H 'content-type: application/json' -d '{"stream":true}' http://127.0.0.1:8080/v1/chat/completions
```

A body with `"stream": true` (the JSON boolean) gets `text/event-stream`, one `data: {...}` event per chunk, each followed by a blank line:

- **chat:** a `chat.completion.chunk` with `delta.role` `"assistant"` (the role chunk), then one chunk per token with `delta.content` set to the token text (`--token-text`, default `" tok"`). The last token's chunk has `finish_reason` `"length"`; the others have `null`.
- **completions:** one `text_completion` chunk per token with `choices[0].text` set to the token text, the last with `finish_reason` `"length"`. There is no role chunk.
- **end:** if the body has `"stream_options": {"include_usage": true}` (the JSON boolean), one more chunk follows with `"choices": []` and `usage` (`prompt_tokens` 8, `completion_tokens` = tokens sent). Then `data: [DONE]`.

This ending follows vLLM 0.32.0's server: the finish reason rides on the last token's chunk, and the usage chunk, without choices, is sent only when the request asks for it ([`completion/serving.py` L458–L492](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/entrypoints/openai/completion/serving.py#L458-L492)). phantom does not send per-chunk usage (`stream_options.continuous_usage_stats`), which vLLM adds to every chunk when it is asked for along with `include_usage`.

Only `stream`, `max_completion_tokens`, `max_tokens` and `stream_options.include_usage` are read from the body; everything else is ignored. Without `"stream": true`, including when the body is not JSON, both endpoints take the non-streaming path and log exactly the old fields.

Tokens per response: `min(limit, --output-tokens)`, where the limit is `max_completion_tokens` if it is a whole number, else `max_tokens` if that is; with neither, `--output-tokens`. So when both are whole numbers, `max_completion_tokens` wins. A limit of 0 still gets one token: a stream always carries at least one.

The token schedule is in [spec.md](spec.md#token-schedule).
