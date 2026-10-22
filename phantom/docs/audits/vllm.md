# Audit: vLLM bench serve

`tests/audit_vllm.rs` runs `vllm bench serve` from [vLLM](https://github.com/vllm-project/vllm) 0.32.0 against an in-process phantom that streams. It compares vLLM's time to first token (TTFT), time per output token (TPOT), inter-token latency (ITL) and end-to-end latency (E2EL) with phantom's per-token ground truth. This is a report, not a pass/fail gate for vLLM. The only assertions are that vLLM exits 0, its result JSON parses, and phantom logs at least 20 requests.

vLLM is an external Python tool. It is installed in a virtual environment outside the repo, and the repo has no Python files or Python dependencies:

```sh
cd ~/audit-tools && uv venv vllm-venv && uv pip install --python vllm-venv vllm   # installed vLLM 0.32.0
```

Run the audit with:

```sh
VLLM_BIN=~/audit-tools/vllm-venv/bin/vllm \
    cargo test --release -p tailbench-phantom --test audit_vllm -- --ignored --nocapture --test-threads=1
```

Without `VLLM_BIN`, or if it does not name a file, the scenarios fail with a message that says so. The test also has five tests that run without vLLM: the `VLLM_BIN` check, the removal of lines without a `client_id` on a hand-written log, intended truth keyed by `client_id` on a hand-written log, reading Python's `Infinity` in vLLM's JSON, and the replica of numpy's percentile on values computed by numpy itself.

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
| V5 | `--request-rate 5 --burstiness inf --max-concurrency 4` | `--stall 10000:2000` | yes, keyed by `client_id` |

Notes on the command:

- **Flags.** All are from `vllm bench serve --help` (0.32.0). The default `--backend openai` posts to `/v1/completions`, which has no role chunk. `--metric-percentiles` takes 100 as well; numpy's 100th percentile is the maximum, so `p100` is vLLM's maximum.
- **`--request-id-prefix ''`.** vLLM sends request `i`'s id as the `x-request-id` header ([`endpoint_request_func.py` L139–L146](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L139-L146)), and the random dataset builds it as prefix + `i` ([`datasets.py` L654–L661](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/datasets/datasets.py#L654-L661)). With an empty prefix the id is `i`, which phantom logs as `client_id`.
- **The token text and the tokenizer.** gpt2 encodes `" the"` as 1 token (id 262), and 32 × `" the"` as 32 tokens. phantom's default `" tok"` is 2 tokens (ids 284 and 74), and 32 × `" tok"` is 64. vLLM re-tokenizes the text only when it reads no `usage` (see (d)). With phantom's current stream ending it reads phantom's `usage`, so the token text does not set the count; `" the"` is kept so that a re-tokenized count would also be 32. Two check runs with the default `" tok"` (2 requests each) show the difference. With phantom's earlier ending, vLLM reported 64 output tokens per request for 32 chunks, and TPOT p99 of 9.85 ms for a 20 ms ITL. With the current ending, it reported 32, and TPOT p99 of 20.00 ms.
- **V5's percentile metrics** are `ttft,tpot,itl,e2el,client_queue_time,e2el_including_client_queue`. vLLM reports the last two only with `--max-concurrency` at a finite rate (see (f)).
- **vLLM's result JSON** is written by Python's `json` module, which writes an infinite `--request-rate` or `--burstiness` as `Infinity`. JSON has no such literal, so the test reads it as a string before parsing.

## What vLLM bench serve measures

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

The request's latency (E2EL) is `most_recent_timestamp - st`: it ends at the last message with non-empty `choices`, not at `[DONE]` ([L287](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/lib/endpoint_request_func.py#L287)). phantom's last message with `choices` is its last token's chunk, which carries `finish_reason` (see [Streaming](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/endpoints.md#streaming)), so each request has 31 ITL entries, one per token gap. phantom's earlier ending sent one more chunk with `choices` after the last token (`text` `""`, `finish_reason` `"length"`, and `usage`), and vLLM took it as one more message: 32 ITL entries per request, the last being the gap from token 31 to that chunk.

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

The wait starts when the request is issued, at its scheduled time: `request_arrival_time = time.perf_counter()` ([L1073](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L1073)). The detailed result lists every request's wait as `queue_times`, in request order ([L1296](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L1296)). With `--max-concurrency` at a finite rate, vLLM also computes a queue-inclusive E2EL, reported as `e2el_including_client_queue` when it is listed in `--percentile-metrics` ([L1318–L1326](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L1318-L1326), [L1398–L1411](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L1398-L1411)):

```python
    queue_times: list[float] | None = None
    e2els_including_queue: list[float] | None = None
    if max_concurrency is not None:
        queue_times = [output.client_queue_time for output in outputs if output.success]
        if not math.isinf(request_rate):
            e2els_including_queue = [
                output.latency + output.client_queue_time
                for output in outputs
                if output.success
            ]
```

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

## Truth

Truth comes from phantom's log through `tests/common/mod.rs` (`stream_lines`, `take_untagged`, `stream_schedule_truth`). Only completed lines with a `client_id` are used.

- **Server TTFT** = `token_ns[0] − arrival_ns`. **Server end-to-end** = `sent_ns − arrival_ns`.
- **Per-gap ITL truth**: every consecutive difference of `token_ns`, pooled over requests (31 per request). Compared with vLLM's ITL.
- **Per-request mean ITL truth**: `(token_ns[last] − token_ns[0]) / (tokens − 1)`, one value per request: the gaps after the first token. Compared with vLLM's TPOT.
- **Intended-schedule truth**, V1, V2 and V5. Request `i`'s intended send time is `origin + i × 200 ms`, with the same origin formula as the other audits (`schedule_origin`: the least-lagged request). In V1 and V2, request `i` is phantom's `seq` `i`. In V5, request `i` is the line whose `client_id` is `i` (`schedule_by_client_id` in the test), because requests waiting for a `--max-concurrency` slot could reach phantom out of order. Intended TTFT = `token_ns[0] − intended`, intended end-to-end = `sent_ns − intended`, and send lateness = `arrival_ns − intended`. vLLM's TTFT and E2EL run from the actual send, and are shown against this truth as well.
- **V5's queue.** vLLM's `client_queue_time` is compared with send lateness. Per request, vLLM's `ttfts[i]` and `latencies[i]`, alone and plus `queue_times[i]`, are compared with intended TTFT and end-to-end of the line whose `client_id` is `i`. vLLM's `e2el_including_client_queue` is compared with intended end-to-end.
- **V3** sends at Poisson times drawn inside vLLM, which phantom does not know, and **V4** is a closed loop with no schedule, so both have server truth only.

Each truth is shown at nearest rank (NR) and at numpy's method (np), the method vLLM uses.

## Results

3 runs, release build, with phantom's current stream ending (the last token's chunk carries `finish_reason`, then a usage chunk with `"choices": []`), all 5 scenarios in each run. Intel(R) Core(TM) i7-14650HX, kernel 6.6.87.2-microsoft-standard-WSL2, rustc 1.94.1 (e408947bf 2026-03-25), tokio 1.53.3, release build (opt-level 3), on AC power, Windows Balanced power plan; Python 3.13.15, vLLM 0.32.0, aiohttp 3.14.5, numpy 2.4.6, transformers 5.19.0. Every cell is the min to max over the 3 runs, in ms. Deltas are vLLM − truth, at nearest rank (NR) and at numpy's method (np). The p100 row is the maximum.

**TTFT** (ms)

| scenario | pct | vLLM | server truth | − server (NR) | − server (np) | intended truth | − intended (NR) | − intended (np) |
|:---|:---|---:|---:|---:|---:|---:|---:|---:|
| V1 | p50 | 102.207–102.448 | 101.086–101.185 | +1.079 to +1.262 | +1.077 to +1.255 | 101.724–101.922 | +0.330 to +0.526 | +0.327 to +0.526 |
|  | p90 | 102.851–102.895 | 101.588–101.761 | +1.089 to +1.277 | +1.088 to +1.272 | 102.945–103.129 | −0.278 to −0.050 | −0.279 to −0.062 |
|  | p99 | 103.149–103.458 | 101.892–102.208 | +1.061 to +1.563 | +1.087 to +1.569 | 103.669–103.998 | −0.540 to −0.295 | −0.506 to −0.277 |
|  | p100 | 103.201–103.709 | 101.980–102.274 | +1.092 to +1.636 | +1.092 to +1.636 | 103.764–104.067 | −0.586 to −0.149 | −0.586 to −0.149 |
| V2 | p50 | 102.090–102.468 | 101.058–101.096 | +1.031 to +1.372 | +1.027 to +1.371 | 101.979–102.107 | +0.020 to +0.370 | +0.006 to +0.359 |
|  | p90 | 103.027–103.450 | 101.909–101.952 | +1.075 to +1.520 | +1.071 to +1.515 | 103.363–103.574 | −0.532 to +0.087 | −0.549 to +0.075 |
|  | p99 | 1702.713–1703.738 | 1798.114–1800.610 | −96.935 to −95.401 | +1.470 to +1.987 | 1800.628–1801.588 | −97.915 to −97.850 | +0.131 to +0.185 |
|  | p100 | 1999.901–2002.120 | 1998.760–2001.100 | +1.020 to +1.141 | +1.020 to +1.141 | 2000.616–2001.584 | −0.715 to +0.565 | −0.715 to +0.565 |
| V3 | p50 | 101.989–102.247 | 101.075–101.164 | +0.913 to +1.084 | +0.913 to +1.082 | — | — | — |
|  | p90 | 102.584–102.929 | 101.649–101.728 | +0.935 to +1.234 | +0.934 to +1.233 | — | — | — |
|  | p99 | 1011.243–1012.137 | 1249.379–1250.075 | −238.627 to −237.893 | +0.871 to +1.064 | — | — | — |
|  | p100 | 1858.518–1859.130 | 1857.242–1858.070 | +1.061 to +1.276 | +1.061 to +1.276 | — | — | — |
| V4 | p50 | 102.422–102.581 | 101.046–101.136 | +1.286 to +1.504 | +1.283 to +1.487 | — | — | — |
|  | p90 | 103.538–103.781 | 101.630–101.731 | +1.908 to +2.050 | +1.907 to +2.050 | — | — | — |
|  | p99 | 104.232–114.943 | 101.859–102.008 | +2.372 to +13.017 | +2.416 to +13.079 | — | — | — |
|  | p100 | 104.339–115.900 | 101.942–102.106 | +2.339 to +13.957 | +2.339 to +13.957 | — | — | — |
| V5 | p50 | 102.084–102.294 | 101.073–101.119 | +0.989 to +1.174 | +0.989 to +1.173 | 605.332–607.241 | −504.947 to −503.217 | −515.277 to −503.705 |
|  | p90 | 102.777–102.952 | 101.649–101.745 | +1.031 to +1.303 | +1.030 to +1.302 | 1768.794–1770.813 | −1668.037 to −1665.842 | −1668.085 to −1666.604 |
|  | p99 | 103.441–103.687 | 102.072–102.199 | +1.369 to +1.488 | +1.437 to +1.650 | 2023.641–2023.986 | −1920.545 to −1919.953 | −1920.179 to −1910.233 |
|  | p100 | 2001.412–2002.335 | 2000.253–2001.293 | +1.041 to +1.159 | +1.041 to +1.159 | 2023.918–2024.442 | −23.030 to −21.686 | −23.030 to −21.686 |

**TPOT** (ms), against the per-request mean gap

| scenario | pct | vLLM | per-request mean truth | − (NR) | − (np) |
|:---|:---|---:|---:|---:|---:|
| V1 | p50 | 20.000–20.004 | 20.000–20.001 | +0.001 to +0.003 | +0.001 to +0.003 |
|  | p90 | 20.021–20.024 | 20.019–20.021 | +0.001 to +0.003 | +0.001 to +0.003 |
|  | p99 | 20.031–20.035 | 20.027–20.034 | −0.001 to +0.008 | +0.001 to +0.009 |
|  | p100 | 20.033–20.038 | 20.030–20.035 | −0.001 to +0.009 | −0.001 to +0.009 |
| V2 | p50 | 20.002–20.005 | 20.001–20.003 | −0.001 to +0.003 | −0.001 to +0.003 |
|  | p90 | 20.021–20.028 | 20.019–20.022 | +0.002 to +0.006 | +0.002 to +0.006 |
|  | p99 | 84.182–84.486 | 84.471–84.501 | −0.289 to −0.015 | +0.003 to +0.006 |
|  | p100 | 84.479–84.535 | 84.474–84.526 | +0.003 to +0.009 | +0.003 to +0.009 |
| V3 | p50 | 20.000–20.003 | 19.997–20.001 | +0.002 | +0.002 |
|  | p90 | 20.020–20.022 | 20.017–20.020 | +0.001 to +0.003 | +0.001 to +0.003 |
|  | p99 | 20.034–20.036 | 20.031–20.034 | +0.002 to +0.003 | +0.003 to +0.006 |
|  | p100 | 84.489–84.499 | 84.491–84.499 | −0.002 to +0.003 | −0.002 to +0.003 |
| V4 | p50 | 20.001–20.005 | 19.997–20.003 | +0.001 to +0.003 | +0.001 to +0.003 |
|  | p90 | 20.021–20.027 | 20.017–20.020 | +0.001 to +0.007 | +0.001 to +0.007 |
|  | p99 | 83.958–84.139 | 83.980–84.146 | −0.073 to −0.007 | +0.007 to +0.010 |
|  | p100 | 84.229–84.255 | 84.218–84.248 | +0.001 to +0.011 | +0.001 to +0.011 |
| V5 | p50 | 20.003–20.006 | 20.001–20.004 | +0.002 to +0.003 | +0.002 to +0.003 |
|  | p90 | 20.024–20.026 | 20.020–20.024 | +0.002 to +0.004 | +0.002 to +0.004 |
|  | p99 | 84.183–84.493 | 84.465–84.512 | −0.283 to −0.018 | +0.002 to +0.004 |
|  | p100 | 84.516–84.541 | 84.515–84.539 | −0.001 to +0.002 | −0.001 to +0.002 |

**ITL** (ms), against every gap

| scenario | pct | vLLM | per-gap truth | − (NR) | − (np) |
|:---|:---|---:|---:|---:|---:|
| V1 | p50 | 20.248–20.266 | 20.254–20.273 | −0.016 to −0.007 | −0.016 to −0.007 |
|  | p90 | 20.448–20.495 | 20.428–20.469 | +0.010 to +0.026 | +0.010 to +0.026 |
|  | p99 | 20.660–20.732 | 20.636–20.697 | +0.024 to +0.046 | +0.025 to +0.047 |
|  | p100 | 21.010–21.569 | 21.014–21.479 | −0.004 to +0.090 | −0.004 to +0.090 |
| V2 | p50 | 20.248–20.270 | 20.259–20.281 | −0.016 to −0.011 | −0.016 to −0.011 |
|  | p90 | 20.487–20.522 | 20.468–20.492 | +0.017 to +0.029 | +0.017 to +0.029 |
|  | p99 | 20.727–20.825 | 20.654–20.716 | +0.056 to +0.108 | +0.057 to +0.109 |
|  | p100 | 2018.976–2020.357 | 2018.845–2020.186 | −0.232 to +0.207 | −0.232 to +0.207 |
| V3 | p50 | 19.894–19.962 | 19.890–19.966 | −0.004 to +0.004 | −0.004 to +0.004 |
|  | p90 | 20.606–20.662 | 20.604–20.647 | +0.002 to +0.015 | +0.002 to +0.015 |
|  | p99 | 20.919–20.943 | 20.893–20.910 | +0.026 to +0.033 | +0.027 to +0.034 |
|  | p100 | 2018.975–2020.527 | 2018.917–2020.584 | −0.116 to +0.058 | −0.116 to +0.058 |
| V4 | p50 | 20.247–20.285 | 20.260–20.297 | −0.027 to −0.008 | −0.027 to −0.009 |
|  | p90 | 20.506–20.594 | 20.491–20.577 | +0.007 to +0.017 | +0.007 to +0.017 |
|  | p99 | 20.738–20.837 | 20.704–20.762 | +0.024 to +0.090 | +0.025 to +0.091 |
|  | p100 | 2010.841–2012.601 | 2010.719–2012.529 | +0.045 to +0.122 | +0.045 to +0.122 |
| V5 | p50 | 20.210–20.254 | 20.236–20.271 | −0.026 to −0.012 | −0.027 to −0.012 |
|  | p90 | 20.524–20.593 | 20.510–20.584 | +0.009 to +0.022 | +0.009 to +0.022 |
|  | p99 | 20.735–20.841 | 20.720–20.814 | +0.014 to +0.052 | +0.014 to +0.061 |
|  | p100 | 2020.636–2020.911 | 2020.633–2020.854 | −0.096 to +0.057 | −0.096 to +0.057 |

**End-to-end** (ms; vLLM E2EL)

| scenario | pct | vLLM | server truth | − server (NR) | − server (np) | intended truth | − intended (NR) | − intended (np) |
|:---|:---|---:|---:|---:|---:|---:|---:|---:|
| V1 | p50 | 722.374–722.419 | 721.163–721.231 | +1.187 to +1.211 | +1.183 to +1.203 | 721.950–722.010 | +0.388 to +0.469 | +0.386 to +0.459 |
|  | p90 | 722.939–723.047 | 721.720–721.830 | +1.180 to +1.250 | +1.179 to +1.249 | 722.935–723.035 | −0.076 to +0.113 | −0.088 to +0.111 |
|  | p99 | 723.268–723.323 | 721.911–722.115 | +1.193 to +1.357 | +1.228 to +1.367 | 723.890–724.024 | −0.726 to −0.567 | −0.719 to −0.496 |
|  | p100 | 723.348–723.467 | 722.050–722.167 | +1.180 to +1.355 | +1.180 to +1.355 | 724.033–724.196 | −0.730 to −0.628 | −0.730 to −0.628 |
| V2 | p50 | 722.159–722.590 | 721.134–721.243 | +1.025 to +1.347 | +1.025 to +1.346 | 721.984–722.206 | +0.175 to +0.384 | +0.173 to +0.374 |
|  | p90 | 723.145–723.503 | 721.942–722.014 | +1.200 to +1.507 | +1.200 to +1.505 | 723.398–724.257 | −0.808 to +0.086 | −0.811 to +0.052 |
|  | p99 | 2711.493–2721.696 | 2719.661–2720.689 | −8.168 to +1.413 | +1.067 to +1.520 | 2720.671–2722.548 | −9.179 to −0.522 | −0.619 to +0.386 |
|  | p100 | 2720.870–2722.850 | 2719.798–2721.652 | +0.948 to +1.198 | +0.948 to +1.198 | 2720.752–2722.573 | +0.118 to +0.277 | +0.118 to +0.277 |
| V3 | p50 | 722.111–722.268 | 721.053–721.146 | +0.971 to +1.122 | +0.969 to +1.120 | — | — | — |
|  | p90 | 722.682–722.913 | 721.683–721.733 | +0.949 to +1.227 | +0.946 to +1.224 | — | — | — |
|  | p99 | 2179.974–2181.034 | 2476.610–2477.790 | −296.798 to −296.636 | +1.031 to +1.209 | — | — | — |
|  | p100 | 2721.520–2721.929 | 2720.561–2720.831 | +0.959 to +1.194 | +0.959 to +1.194 | — | — | — |
| V4 | p50 | 722.457–722.619 | 721.058–721.087 | +1.370 to +1.557 | +1.365 to +1.555 | — | — | — |
|  | p90 | 723.681–724.049 | 721.651–721.746 | +2.014 to +2.304 | +2.013 to +2.301 | — | — | — |
|  | p99 | 2705.050–2710.078 | 2704.376–2709.452 | −0.971 to +0.728 | +1.097 to +1.332 | — | — | — |
|  | p100 | 2712.837–2713.991 | 2711.511–2712.802 | +0.981 to +1.326 | +0.981 to +1.326 | — | — | — |
| V5 | p50 | 722.212–722.487 | 721.084–721.217 | +1.067 to +1.318 | +1.065 to +1.312 | 1246.321–1250.253 | −527.767 to −524.037 | −555.106 to −552.462 |
|  | p90 | 722.894–723.121 | 721.713–721.789 | +1.155 to +1.407 | +1.150 to +1.403 | 2410.913–2412.840 | −1689.831 to −1687.972 | −1695.273 to −1693.471 |
|  | p99 | 2712.139–2721.887 | 2719.956–2721.452 | −8.474 to +0.951 | +0.994 to +1.311 | 2721.484–2722.098 | −9.613 to +0.403 | +0.240 to +0.656 |
|  | p100 | 2722.287–2722.683 | 2721.121–2721.645 | +0.975 to +1.166 | +0.975 to +1.166 | 2721.541–2722.268 | +0.206 to +0.747 | +0.206 to +0.747 |

**Requests, ITL samples and send lateness**

| scenario | vLLM completed / failed | phantom completed / `completed: false` / no client_id | vLLM ITL samples | phantom gaps | lateness p50 ms | lateness p99 ms | lateness max ms |
|:---|---:|---:|---:|---:|---:|---:|---:|
| V1 | 150/0 | 150/0/0 | 4650 | 4650 | 0.658–0.795 | 2.340–2.710 | 2.383–2.807 |
| V2 | 150/0 | 150/0/0 | 4650 | 4650 | 0.665–0.901 | 2.482–2.828 | 2.584–10.743 |
| V3 | 150/0 | 150/0/0 | 4650 | 4650 | — | — | — |
| V4 | 150/0 | 150/0/0 | 4650 | 4650 | — | — | — |
| V5 | 150/0 | 150/0/0 | 4650 | 4650 | 447.652–451.836 | 1922.552–1923.151 | 1922.625–1923.219 |

**V5: vLLM's client_queue_time** (ms), against phantom's send lateness (arrival − intended)

| pct | vLLM client_queue_time | lateness truth | − (NR) | − (np) |
|:---|---:|---:|---:|---:|
| p50 | 475.051–478.368 | 447.652–451.836 | +26.532 to +27.715 | −0.925 to −0.726 |
| p90 | 1616.715–1619.045 | 1612.222–1613.728 | +4.493 to +5.317 | −1.229 to −0.139 |
| p99 | 1912.452–1921.948 | 1922.552–1923.151 | −10.281 to −0.604 | −0.581 to −0.432 |
| p100 | 1922.147–1922.655 | 1922.625–1923.219 | −0.979 to −0.136 | −0.979 to −0.136 |


**V5: vLLM's queue-inclusive E2EL** (`e2el_including_client_queue`, ms), against intended truth

| pct | vLLM | intended truth | − (NR) | − (np) |
|:---|---:|---:|---:|---:|
| p50 | 1275.307–1278.336 | 1246.321–1250.253 | +28.082 to +29.077 | +0.562 to +0.910 |
| p90 | 2416.833–2418.203 | 2410.913–2412.840 | +5.363 to +6.498 | −0.079 to +0.999 |
| p99 | 2712.207–2721.946 | 2721.484–2722.098 | −9.545 to +0.463 | +0.309 to +0.708 |
| p100 | 2722.339–2722.734 | 2721.541–2722.268 | +0.260 to +0.799 | +0.260 to +0.799 |

**V5, per request: vLLM − intended truth for the same request** (ms)

| quantity | min | p50 | p99 | max |
|:---|---:|---:|---:|---:|
| vLLM TTFT alone | −1922.428 to −1921.748 | −505.121 to −503.167 | +0.814 to +1.265 | +0.822 to +1.352 |
| vLLM TTFT + queue | −1.348 to −0.905 | +0.251 to +0.557 | +0.935 to +1.321 | +0.971 to +1.417 |
| vLLM E2EL alone | −1922.267 to −1921.627 | −504.962 to −502.870 | +0.826 to +1.130 | +0.869 to +1.141 |
| vLLM E2EL + queue | −1.316 to −0.933 | +0.352 to +0.647 | +0.958 to +1.265 | +1.027 to +1.311 |

These ranges are the observed min–max over the 3 runs, not bounds: with only 3 runs a new run falls outside the range about half the time by chance, and this audit sets no pass criteria at all — it asserts only that vLLM exited 0, its result JSON parsed, and phantom logged at least 20 requests.

In every run, vLLM completed all 150 requests with none failed, and phantom logged 150 completed lines with none abandoned. Every response had 32 tokens on both sides, and vLLM's count came from phantom's `usage`. vLLM's ITL list had 31 entries per request (4650), the same as phantom's gaps. In V1 and V2, no request arrived more than one period (200 ms) after its intended send time.

What the tables show:

- **TTFT against server truth.** At numpy's method, vLLM − server TTFT was +1.08 to +1.64 ms in V1, +1.02 to +1.99 ms in V2, +0.87 to +1.28 ms in V3 and +0.99 to +1.65 ms in V5: +0.87 to +1.99 ms over those four. In V4 it was +1.28 to +2.60 ms, except p99 and max in run 2: +13.08 and +13.96 ms. At nearest rank the deltas were in the same ranges, except at p99 in V2 (−95.4 to −96.9 ms) and V3 (−237.9 to −238.6 ms).
- **Percentile method in the stall tail.** At p99, numpy's virtual index `(n − 1) × 0.99` = 147.51 lies between the 148th and 149th smallest of 150 values, and nearest rank takes the 149th. In V2 and V3 both are requests the stall delayed, by different amounts. numpy returns the point 51% of the way from the first to the second, a value no request had. vLLM's TTFT p99 was 1702.7–1703.7 ms in V2 (nearest-rank truth 1798.1–1800.6 ms). Its TTFT and E2EL p99 in V3 were 1011.2–1012.1 and 2180.0–2181.0 ms (nearest-rank truth 1249.4–1250.1 and 2476.6–2477.8 ms). The same effect, smaller, shows at nearest rank in E2EL p99 against server truth in V2 (−8.2 to +1.4 ms) and V5 (−8.5 to +0.95 ms), and in TPOT p99 in V2 (−0.02 to −0.29 ms) and V5 (−0.02 to −0.28 ms). Against truth taken at numpy's method, all of these cells are within the ranges of the other cells.
- **TTFT against intended truth (V1, V2).** vLLM − intended TTFT was −0.72 to +0.57 ms at both methods, except V2 p99 at nearest rank (−97.85 to −97.92 ms, the tail effect). Send lateness was 0.66–0.90 ms at p50 and 2.34–2.83 ms at p99. Its maximum was 2.38–3.63 ms, except 10.74 ms in one V2 run.
- **TPOT.** vLLM's TPOT matched the per-request mean truth within −0.002 to +0.011 ms at numpy's method in every cell. A stalled request's TPOT was about 84 ms: a per-request mean spreads its 2 s gap over 31 gaps.
- **ITL.** vLLM's ITL matched per-gap truth within −0.027 to +0.109 ms at every percentile from p50 to p99, and within −0.232 to +0.207 ms at the maximum. In V2 to V5, the maximum ITL was the stall itself, 2.01–2.02 s. At p50, vLLM's ITL was 0.007–0.027 ms below truth in V1, V2, V4 and V5, and −0.004 to +0.004 ms from it in V3.
- **End-to-end against server truth.** vLLM − server was +0.95 to +2.30 ms in every cell at numpy's method. At nearest rank, the p99 cells of V2, V3 and V5 show the tail effect (above), and V4 p99 was −0.97 to +0.73 ms; the other nearest-rank cells were +0.95 to +2.30 ms.
- **End-to-end against intended truth (V1, V2).** vLLM − intended was −0.81 to +0.46 ms at numpy's method, and −0.81 to +0.47 ms at nearest rank except V2 p99 (−0.52 to −9.18 ms, the tail effect).
- **Percentile method elsewhere.** In V1 to V4, outside the cells named above (V2 TTFT, E2EL and TPOT p99; V3 TTFT and E2EL p99), nearest rank and numpy's method gave truths at most 2.16 ms apart (V4 end-to-end p99, one run). At p100 both are the maximum.

**V5: a schedule behind `--max-concurrency`.** V5 sends on V2's schedule but runs at most 4 requests at once. In every run, `client_id` equalled `seq` on all 150 lines, so keying by `client_id` gave the same truth as keying by `seq` would have.

- **Against server truth,** V5 read like V1 and V2: TTFT +0.99 to +1.65 ms and end-to-end +0.98 to +1.40 ms at numpy's method, TPOT within −0.001 to +0.004 ms, and ITL within −0.096 to +0.061 ms.
- **Requests waited for a slot.** 99 of 150 requests waited 1 ms or more in every run (`queue_times` in vLLM's detailed result), and 89–90 of 150 reached phantom more than one period after their intended send time. Send lateness was 447.7–451.8 ms at p50 and 1922.6–1923.2 ms at p99 and max.
- **vLLM's TTFT and E2EL against intended truth.** vLLM's TTFT p50 was 102.1–102.3 ms; intended TTFT p50 was 605.3–607.2 ms at nearest rank. vLLM − intended TTFT was −503.2 to −504.9 ms at p50 and −1919.95 to −1920.55 ms at p99 (nearest rank). vLLM's E2EL p50 was 722.2–722.5 ms; intended end-to-end p50 was 1246.3–1250.3 ms. Per request, vLLM's TTFT alone was down to 1921.7–1922.4 ms below intended TTFT.
- **client_queue_time against send lateness.** vLLM's client_queue_time was 475.1–478.4 ms at p50, 1912.5–1921.9 ms at p99 and 1922.1–1922.7 ms at max. Against phantom's send lateness at numpy's method it was −0.14 to −1.23 ms at every percentile.
- **TTFT + client_queue_time against intended TTFT, per request.** For every request in every run, vLLM's TTFT plus its client_queue_time was within −1.35 to +1.42 ms of the same request's intended TTFT (p50 +0.25 to +0.56 ms). E2EL plus client_queue_time was within −1.32 to +1.31 ms of intended end-to-end.
- **vLLM's queue-inclusive E2EL** (`e2el_including_client_queue`, which vLLM reports with `--max-concurrency` at a finite rate) was −0.08 to +1.00 ms from intended end-to-end at numpy's method.
- **Percentile method.** V5's queue times and lateness are spread out: neighbouring values around p50 differ by tens of ms. Nearest rank and numpy's method therefore differ by up to 29 ms in the client_queue_time, queue-inclusive E2EL and intended-truth cells (for example, client_queue_time − lateness at p50: +26.5 to +27.7 ms at nearest rank, −0.73 to −0.93 ms at numpy's method). The comparisons above use numpy's method, vLLM's own.

**Earlier sets of runs.** Two earlier sets of 3 runs are not tabulated.

- **phantom's earlier stream ending.** After the last token it sent one more chunk with `choices` (`text` `""`, `finish_reason` `"length"`) and `usage`. vLLM's ITL list then had 32 entries per request (4800 against 4650 gaps). The extra entry, the gap from the last token to that chunk, was 0.005–0.006 ms at p50 and at most 0.201 ms. vLLM re-tokenized the text instead of reading `usage`; with `" the"` the count was the same, 32. vLLM's ITL p50 was further below truth: −0.020 to −0.041 ms in V1 and V2 (tabulated: −0.007 to −0.016), and −0.006 to −0.020 ms in V3 (tabulated: −0.004 to +0.004). Of the 160 delta cells of V1 to V4, 134 have min–max ranges that overlap with the tabulated runs, and the other 26 are apart by at most 0.112 ms. The ITL count and the token source changed as the source read in (a) and (d) predicts.
- **The current ending, a day before the tabulated runs.** In runs 1 and 3 of that set, V2's TTFT p99 and max were +2.49 to +4.38 ms against server truth, and V2 had two single ITL maxima of +1.31 and +2.43 ms; V1 had one of +0.695 ms. In the tabulated runs these cells were at most +1.99 ms (TTFT) and +0.21 ms (ITL). V4's TTFT p99 and max in run 2 of the tabulated runs (+13.08 and +13.96 ms) had no counterpart in either earlier set, where they were at most +3.35 ms.

## Candidate explanations

Candidate explanations, none of them verified:

- **What each side's clock covers.** vLLM's TTFT and E2EL start just before aiohttp posts the request and end when its client has parsed the message. phantom's truth starts at handler entry and ends at the chunk hand-off. vLLM's side also includes aiohttp writing the request, the loopback trip both ways, hyper's write, and reading and parsing each SSE message in Python. That would put a positive delta on TTFT and end-to-end, as seen (about +1 ms; GuideLLM's was about +0.5 ms). V4 has 4 requests in flight on one event loop, which would add more.
- **TTFT and E2EL against intended truth.** vLLM's values run from its actual send, intended truth from the intended send. Intended truth therefore includes the send lateness (0.66–0.90 ms at p50 in V1 and V2), and vLLM's values include the client-side time in the first candidate (about 1 ms). The two partly cancel, which would put the deltas near 0, as seen.
- **ITL p50 slightly below truth.** vLLM's gap is between the times its client parsed two messages, and phantom's between two hand-offs. A message read late lengthens one gap and shortens the next. That widens vLLM's gap distribution, which raises the upper percentiles (as seen at p90 and p99) and can lower the middle. The earlier ending's near-zero extra gaps (3% of the samples) lowered p50 further. This was not examined.
- **V5's backlog.** Each request takes about 720 ms (TTFT 100 ms + 31 × 20 ms), so at 5 requests/s about 3.6 are in flight, under the limit of 4, until the stall. The stall holds all 4 slots for up to 2 s while sends continue, and afterwards 4 slots complete about 5.6 requests/s against 5 sent per second. The backlog would then shrink by only about 0.6 requests/s, and so last to the end of the run. That would match the about 100 requests (from about request 50, at 10 s) that waited. It was not checked request by request.
- **TTFT + client_queue_time near intended TTFT.** vLLM's client_queue_time starts when the request is issued (`request_arrival_time = time.perf_counter()`, [`serve.py` L1073](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/benchmarks/serve.py#L1073)), at its scheduled time plus the event loop's own delay, and TTFT ends when vLLM's client has parsed the first token. The sum therefore runs from about the intended send to the client's read, and the remaining ±1.4 ms would be the issue delay and the client-side time in the first candidate. This was not examined further.
- **Single large values.** V4's TTFT p99 and max in one run (about +13 ms), and in the earlier set V2's stall tail (up to +4.4 ms) and a few single ITL maxima (up to +2.4 ms). For V2's stall tail: when the stall window ends, phantom releases at once the held token of every stream in the window, so vLLM's one event loop reads many streams' messages in a burst, and messages read later get later timestamps. For the others, a pause in vLLM's event loop or in reading the socket is a candidate. None of these was examined.

## Limits

Limits:

- **Loopback only.** vLLM and phantom share one machine and one kernel. Nothing here measures a real network.
- **One chunk = one token.** phantom sends one `" the"` per chunk and reports one completion token per chunk in `usage`, which vLLM reads. A real server may put several tokens in one chunk; vLLM's ITL would then be per chunk, not per token. The token text is one gpt2 token, so a re-tokenized count would match too.
- **Completions endpoint only.** On the chat backend, vLLM would also take phantom's role chunk as the first token and add one more ITL entry for it (a). Chat was not run.
- **Truth is server-side plus intended-schedule.** Server truth stops at phantom's chunk hand-off: it cannot see the response being written, carried or read. Intended truth depends on the origin formula and on the request order.
- **Intended truth for constant spacing only.** V3's Poisson send times are drawn inside vLLM from its seed, and phantom does not know them. V4 has no schedule. Both have server truth only.
- **Request order.** In V1 and V2, intended truth is keyed by seq, and every line's `client_id` (vLLM's request index) equalled its `seq` in every run, so seq order was vLLM's send order. V5's is keyed by `client_id`, and also matched seq. In V4, with 4 requests in flight and no schedule, `client_id` equalled `seq` on 99–102 of 150 lines.
- **client_queue_time is compared per request through `client_id`.** vLLM's detailed result lists `ttfts`, `latencies` and `queue_times` in request order, so entry i is request i, whose `x-request-id` is i (see the notes on the command).
- **Earlier sets are summarized, not tabulated.** Their 3 runs each are compared above in a few figures only.
- **Release build, one machine, 3 runs.**
