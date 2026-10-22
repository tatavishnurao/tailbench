# Audits

A latency harness can only be audited against something that knows what really happened. phantom follows a script fixed at startup and writes what it did for each request to a JSONL log, so a harness's measurements can be compared request by request with the truth.

The reports:

- [tailbench](tailbench.md): `tests/audit.rs`
- [oha](oha.md): `tests/audit_oha.rs`
- [GuideLLM](guidellm.md): `tests/audit_guidellm.rs`
- [vLLM bench serve](vllm.md): `tests/audit_vllm.rs`

How phantom's log is turned into truth is indexed in [truth-log.md](../truth-log.md#audit-truth).

## oha, GuideLLM and vLLM compared

Only what the three source reads linked below established. "Not read" means this repo's audits did not read that part of the tool, not that the tool lacks it. The oha audit was non-streaming, so oha's streaming metrics were not read.

The three source reads: [oha](oha.md), [What GuideLLM measures](guidellm.md#what-guidellm-measures), [What vLLM bench serve measures](vllm.md#what-vllm-bench-serve-measures).

| | oha 1.16.0 | GuideLLM 0.8.1 | vLLM bench serve 0.32.0 |
|:---|:---|:---|:---|
| TTFT | not read | from the request's header write to the first SSE line with non-empty content (`delta.content`, reasoning or a tool call); a chat role chunk with `content` `""` is not counted | from just before the request is posted to the first SSE message with non-empty `choices`, whatever its text; on chat, a role chunk counts |
| ITL | not read | per request: `(last token line − first token line) / (output tokens − 1)`; its distribution weights each request by `output tokens − 1` | every gap between consecutive messages with non-empty `choices`, pooled over requests; a message with `choices` counts even if its text is empty |
| TPOT | not read | not read | per request: `(latency − TTFT) / (output tokens − 1)`, unweighted |
| Output tokens | not applicable | asks for usage (`include_usage` and `continuous_usage_stats`); the last `usage.completion_tokens` seen, else the number of token-bearing lines | asks for usage (`include_usage`); the server's `usage.completion_tokens` from a message without `choices`, else the generated text re-tokenized with `--tokenizer` |
| End of a request | not read | the end of the stream | the last message with non-empty `choices`, not `[DONE]` |
| Corrected or scheduled latency | `--latency-correction`: "Correct latency to avoid coordinated omission problem. It's ignored if -q is not set." (`oha --help`) | `request_scheduled_latency`, constant profile only: request end − targeted start, `start + index / rate` | TTFT, TPOT, ITL and E2EL run from the actual send. The wait for `--max-concurrency` is recorded separately as `client_queue_time`, from the request's issue at its scheduled time; with `--max-concurrency` at a finite rate, `e2el_including_client_queue` = E2EL + `client_queue_time` |
| Percentile | sorted values at the truncated index `(p / 100 × n) as usize`, no interpolation | first value whose weighted running sum (numpy `cumsum`) is ≥ q (`searchsorted`, `side="left"`) | `np.percentile`, linear interpolation between the two samples around `(n − 1) × p / 100` |
| Clock | not read | `time.time()`, a wall clock | `time.perf_counter()` for request timings; `time.time()` for the send schedule |
