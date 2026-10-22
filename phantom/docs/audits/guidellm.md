# Audit: GuideLLM

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

## What GuideLLM measures

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

## Truth

Truth comes from phantom's log through `tests/common/mod.rs` (`stream_lines`, `stream_schedule_truth`). Only completed lines are used; `"completed": false` lines are counted but kept out.

- **Server TTFT** = `token_ns[0] − arrival_ns`. **Server end-to-end** = `sent_ns − arrival_ns`.
- **ITL truth**, two ways. The **per-request mean** is `(token_ns[last] − token_ns[0]) / (tokens − 1)`, weighted by `tokens − 1` like GuideLLM's. **Pooled gaps** are every consecutive difference of `token_ns`.
- **Intended-schedule truth**, constant profile only. Request `i` is phantom's `seq` `i`, and its intended send time is `origin + i × 200 ms`, with the same origin formula as the other audits (`schedule_origin`: the least-lagged request). Intended TTFT = `token_ns[0] − intended`, intended end-to-end = `sent_ns − intended`, and send lateness = `arrival_ns − intended`.
- **G3** is synchronous: it has no intended schedule. It shows server truth and, as a cross-run reference, G2's intended-schedule truth from the same test process. TTFT is compared with G2's intended TTFT, and GuideLLM's `request_latency` with G2's intended end-to-end.

## Results

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

## Re-run without the workarounds

Once phantom served `/health` and read `max_completion_tokens`, the audit was run once more, in release, on the same machine, with the backend as in the command above: GuideLLM's own health check and token limit.

- **Exit and tokens.** All 4 scenarios exited 0. Every response carried 32 tokens: GuideLLM's output token count was 32 (mean, min and max) in every scenario, and so was phantom's per completed line.
- **Request counts.** They matched in G1, G3 and G4 (147/4, 38/1, 147/4). In G2, GuideLLM reported 147 successful and 3 incomplete (150 requests in total), and phantom logged 147 completed lines and 4 `"completed": false` lines.
- **Against the 3-run ranges.** 310 of the 453 printed values (truths, GuideLLM values and deltas) fell outside the ranges in the tables above, most by under 0.2 ms. The ranges are not bounds, and none of this is a failure. The values that moved most:
  - **Send lateness** p99 was 16.3 ms in G1 and 4.7–4.8 ms in G2 and G4 (the tables: 22.3–29.3 ms); max was 17.2 ms in G1 and 5.0–5.4 ms in G2 and G4 (23.5–29.9 ms). p50 was 2.0–2.2 ms.
  - **TTFT − intended truth** at p99 and max was −14.3 and −13.8 ms in G1 and −3.9 and −4.3 ms in G4 (the tables: −21.4 to −25.9 ms). G2 at p90 was −2.6 ms (−20.4 to −23.2 ms).
  - **Scheduled end-to-end − intended truth** was +1.9 to +3.0 ms (the tables: +1.4 to +2.5 ms).
  - **G2's stalled requests.** TTFT p99 was 1801.1 ms and max 2002.7 ms (the tables: 1821–1828 and 2022–2027 ms). Server truth moved with them (1800.7 and 2001.9 ms), so GuideLLM − server was +0.39 and +0.74 ms.
  - **Against server truth,** the largest deltas were G1 TTFT max +2.25 ms and G1 end-to-end p99 +1.95 ms (the tables: up to +0.93 and +1.38 ms).

## Re-run with vLLM's stream ending

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

## Candidate explanations

Candidate explanations, none of them verified:

- **What each side's clock covers.** GuideLLM's TTFT and `request_latency` start at its header write and end when its client has read the line. phantom's truth starts at handler entry and ends at the chunk hand-off. GuideLLM's side also includes httpx writing the request, the loopback trip both ways, hyper's write, and the client reading and parsing each SSE line. That would put a small positive delta on TTFT and end-to-end, as seen.
- **Actual send against intended send.** GuideLLM's TTFT is measured from its actual header write, and intended TTFT from the intended send time. Send lateness is in the second but not the first, which would explain the negative TTFT deltas against intended truth, largest where lateness is largest (p99). GuideLLM reports no scheduled TTFT, so there is no like-for-like column for it.
- **The intended origin.** The origin carries the smallest send-to-handler delay of the run, and GuideLLM's `targeted_start` is its own schedule on its own clock. The two schedules can differ by a fixed offset, which would appear as a constant in the scheduled end-to-end delta (+1.4 to +2.5 ms).
- **Send lateness of about 20–30 ms for a few requests.** GuideLLM ran 10 worker processes for the constant profile (`worker_count` 10 in its JSON). Opening a new connection, or process scheduling, could make a few sends late. This audit did not identify which requests were late or why.
- **Lower send lateness without `validate_backend: false`.** Each GuideLLM worker process calls its backend's `validate()` at startup, before the workers synchronise ([`worker.py` L284–L289](https://github.com/vllm-project/guidellm/blob/v0.8.1/src/guidellm/scheduler/worker.py#L284-L289)), and with validation on, that is a `GET /health` through the worker's HTTP client. If that leaves a connection open, a worker's first timed request no longer opens one. This would lower the lateness of the few late sends, and with it the TTFT and scheduled end-to-end differences against intended truth. It was not checked, and one run is one sample.
- **One request count off by one in each re-run (G2, then G1).** GuideLLM counted 150 requests and phantom logged 151. One request reached phantom's handler and was abandoned, but is not in GuideLLM's totals. A request started at the 30 s deadline and dropped by GuideLLM's measurement window is a candidate. It moved between scenarios, so it is not tied to one profile. The runs did not record which request it was.
- **G3's TTFT maximum in the second re-run (+7.85 ms).** A single request whose first token GuideLLM recorded about 7.9 ms after phantom's hand-off. As for G1's end-to-end maximum below, event-loop or garbage-collector pauses and wall-clock steps are candidates. The change of stream ending comes after the first token, so it is not a candidate here.
- **One late end-to-end maximum (G1, +9.21 ms).** A single request whose end GuideLLM recorded about 9 ms after phantom's `[DONE]` hand-off. Python event-loop or garbage-collector pauses in a worker are candidates. So is a step in the wall clock, since GuideLLM's timestamps are `time.time()`, not a monotonic clock.

## Limits

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
