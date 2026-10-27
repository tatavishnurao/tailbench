# Real server freeze

**Question:** when a real inference server freezes for 2.0 s, what does each load tool report about latency?

The server is vLLM on the local GPU. The freeze is `kill -STOP` on the server's process group, then `kill -CONT` 2.0 s later. The orchestrator logs both wall-clock times. That log is the reference; no benchmark tool controls it. [`chart.html`](chart.html) shows the results (open it in a browser).

- GPU: NVIDIA GeForce RTX 4060 Laptop GPU, 8188 MiB, driver 617.42 (WSL2). Model: `Qwen/Qwen2.5-0.5B-Instruct`, vLLM 0.32.0, `--max-model-len 2048 --gpu-memory-utilization 0.5`.
- Load: 10 req/s for 25 s (250 requests), non-streaming `POST /v1/completions` with `"Hello"` and `max_tokens 16` (tools that generate their own prompts use about 64 input and 16 output tokens).
- Freeze: 2.0 s, started 8 s after the first request the server logged. Each configuration ran twice per pass.

## Headline

Pass 2 is the headline pass (why: see "Two passes"); the table lists both repetitions of each configuration and pass 1 next to it. Full per-run tables follow the method.

<!-- BEGIN:headline -->
| cfg | tool | reported p99 ms, pass 2 (rep 1 / rep 2) | reported max ms, pass 2 | expected p99 ms (phase-avg) | reported p99 ms, pass 1 | oha connection errors, pass 2 |
|:--|:--|:--|:--|--:|:--|:--|
| A | tailbench open loop (rs-drive) | 2037 / 2107 | 2231 / 2268 | 1927 | 2085 / 1956 | - |
| B | tailbench closed loop (rs-drive) | 105 / 170 | 2178 / 2214 | 99 | 109 / 135 | - |
| C | oha -c 1 | 116 / 173 | 2165 / 2093 | 97 | 130 / 122 | 0 / 0 |
| D | oha -c 1 --latency-correction | 2261 / 2794 | 2282 / 4130 | 2160 | 2211 / 2331 | 0 / 0 |
| C2 | oha -c 50 | 178 / 2101 | 2111 / 2266 | 1927 | 108 / 181 | 19 / 0 |
| D2 | oha -c 50 --latency-correction | 183 / 159 | 2171 / 2095 | 1927 | 102 / 174 | 19 / 20 |
| E | vllm bench serve | 2084 / 2077 | 2307 / 2243 | 1935 | 2076 / 2104 | - |
| F | GuideLLM constant | 2203 / 2272 | 2445 / 3109 | 1921 | 2195 / 2195 | - |
| G | AIPerf constant | 2145 / 2128 | 2358 / 2346 | 1925 | 2075 / 2158 | - |
<!-- END:headline -->

**Observations**

1. **Tools that send on a schedule and have no concurrency cap that this load reaches (A, E, F, G) put the freeze in their p99.** Pass 2 p99 is 2037-2272 ms and pass 1 is 1956-2195 ms, against a phase-averaged expectation of 1920-1935 ms; the reported values sit 109-350 ms above the expectation in pass 2. The same tools report a max of 2231-3109 ms.
2. **A single connection hides the freeze unless the tool corrects for it.** oha `-c 1` and rs-drive closed loop (C, B) report a p99 of 105-173 ms in pass 2 (expected FIFO p99 94-101 ms) and show the freeze only in the max (2093-2214 ms). The same oha command with `--latency-correction` (D) reports, in pass 2 reps 1 / 2, a p50 of 2050 / 2007 ms and a p99 of 2261 / 2794 ms; the FIFO intended-time model expects a p50 of 1728 / 637 ms and a p99 of 2172 / 2147 ms. With one connection the queue is in the client; at an `s0` near the 100 ms period (96.4 and 87.7 ms in these two runs) the backlog from the freeze does not drain, so the corrected numbers include the client's own queue and exceed A's (A's pass 2 p50 is 88 / 102 ms, p99 2037 / 2107 ms). D's max in rep 2 was 4130 ms against an expected 2180 ms and is listed as unexplained below.
3. **oha with 50 connections reported a p99 near baseline in 7 of 8 runs while counting 19-20 `connection error` entries per run.** Those runs have p99 of 102-183 ms against an expected 1919-1933 ms. oha's own latency statistics exclude errored requests (see "How oha treats errors"). The one run with no connection errors reported 2101 ms. Why the server closed those connections is an open question (see "Open question: keep-alive").

The ratio between the tools that show the freeze in their p99 and those that do not is about 15x (A's mean pass 2 p99 is 2072 ms, B's is 137 ms). The conclusions rest on that ratio, not on absolute milliseconds: the `s0` drift (unexplained; see "Thermals") moves absolute values by tens of ms.

## How to run

```sh
cd driver && cargo build --release && cd ..   # rs-drive, the tailbench driver
python3 run.py                                # about 15 min; writes out/ (gitignored)
python3 build_results.py                      # results.json, and the tables in this README
python3 chart.py                              # chart.html
python3 expected.py                           # the analytic reference on its own
```

Tools live outside the repo in `~/audit-tools/{vllm,guidellm,aiperf}-venv`; oha is `~/.cargo/bin/oha` 1.16.0. `diag.py` holds the one-off diagnostics described below. Python is standard library only.

## Two passes

The whole matrix ran twice, back to back (`out/pass1/` and `out/`). Between them I changed `run.py` to measure the baseline service time `s0` with plain requests before and after every run, because the p50s the tools reported in pass 1 ranged from about 80 to 107 ms and the single `s0` I had used for the expected values had been measured earlier. Pass 1 has no per-run `s0`.

**The headline tables use pass 2.** Reason: its expected values use an `s0` measured for that run, independent of any tool. I chose it after seeing pass 1, and that is a choice I made, so pass 1 is reported in full below. Both passes show the same shapes; they differ in the oha `-c 50` runs and in the `-c 1` counts (see below).

`s0` as measured (ms):

<!-- BEGIN:s0 -->
| s0 measurement | method | median ms | min ms | max ms |
|:--|:--|--:|--:|--:|
| gate (STEP 0) | 5 requests, mean | 91.4 | 91.4 | 91.4 |
| pass 1: at server start | median of 10 | 86.2 | 86.2 | 86.2 |
| pass 1: per run | not measured | - | - | - |
| pass 2: at server start | median of 10 | 85.5 | 85.5 | 85.5 |
| pass 2: per run, before | median of 5, n=18 | 86.9 | 82.9 | 101.1 |
| pass 2: per run, after | median of 5, n=18 | 99.8 | 86.7 | 114.7 |
<!-- END:s0 -->

`s0` drifted: 91.4 ms at the gate, 86.2 at pass 1's start, 85.5 at pass 2's start, and within pass 2 the median is 86.9 ms before a run and 99.8 ms after it (the "after" values reach 114.7). Servers started later for the diagnostics had baselines of 93.1, 104.0, 102.1 and 117.9 ms, and in the 7-run soak the tools' p50 rose from 118 ms to 1506 ms. I did not find the cause; the machine is a laptop under WSL2 and I did not control GPU clocks or host load. At 10 req/s a service time near 100 ms means a single connection is at about 100% utilisation, which matters for the `-c 1` rows.

## Results, pass 2 (headline)

`n ok` is the number of requests the tool counted in its latency statistics. For oha it is its `statusCodeDistribution` total. The expected columns come from [`expected.py`](expected.py) (no tool involved): `unlimited` model for A, C2, D2, E, F, G; a FIFO single-server simulation for B, C, D (uncorrected rows compare to service-time latency, D to intended-time latency). The expected p99 here is phase-averaged over the unknown schedule origin; for A the exact origin is known and `results.json` holds `expected_exact` (A p99: reported 2037 / 2107 ms, exact 1889 / 1973 ms).

<!-- BEGIN:main_pass2 -->
| cfg | tool | rep | n ok | errors | p50 ms | p99 ms | max ms | expected p99 (phase-avg) | expected max | s0 used ms |
|:--|:--|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| A | tailbench open loop (rs-drive) | 1 | 250 | 0 | 88.3 | 2036.7 | 2230.8 | 1927.3 | 2127.3 | 93.0 |
| B | tailbench closed loop (rs-drive) | 1 | 250 | 0 | 92.3 | 104.6 | 2178.0 | 99.2 | 2185.7 | 99.2 |
| C | oha -c 1 | 1 | 227 | 1 deadline | 99.1 | 115.5 | 2164.6 | 101.1 | 2184.9 | 101.1 |
| D | oha -c 1 --latency-correction | 1 | 227 | 1 deadline | 2049.6 | 2260.8 | 2281.6 | 2172.3 | 2179.6 | 96.4 |
| C2 | oha -c 50 | 1 | 230 | 19 conn + 3 deadline | 105.8 | 178.2 | 2111.1 | 1931.9 | 2131.9 | 96.6 |
| D2 | oha -c 50 --latency-correction | 1 | 230 | 19 conn + 3 deadline | 102.4 | 182.7 | 2171.2 | 1930.3 | 2130.3 | 93.6 |
| E | vllm bench serve | 1 | 250 | 0 | 98.3 | 2084.3 | 2307.5 | 1935.0 | 2135.0 | 101.1 |
| F | GuideLLM constant | 1 | 250 | 0 | 103.5 | 2203.0 | 2445.4 | 1921.0 | 2121.0 | 86.9 |
| G | AIPerf constant | 1 | 249 | 0 | 95.4 | 2145.0 | 2358.0 | 1921.4 | 2121.4 | 88.4 |
| A | tailbench open loop (rs-drive) | 2 | 250 | 0 | 101.6 | 2106.9 | 2268.1 | 1927.1 | 2127.1 | 92.8 |
| B | tailbench closed loop (rs-drive) | 2 | 250 | 0 | 100.6 | 169.9 | 2213.8 | 97.8 | 2181.8 | 97.8 |
| C | oha -c 1 | 2 | 218 | 1 deadline | 97.5 | 173.3 | 2093.1 | 93.8 | 2176.8 | 93.8 |
| D | oha -c 1 --latency-correction | 2 | 208 | 1 deadline | 2007.1 | 2794.0 | 4129.8 | 2147.0 | 2171.5 | 87.7 |
| C2 | oha -c 50 | 2 | 249 | 3 deadline | 100.0 | 2100.9 | 2265.8 | 1922.7 | 2122.7 | 90.2 |
| D2 | oha -c 50 --latency-correction | 2 | 229 | 20 conn + 3 deadline | 100.8 | 159.2 | 2095.5 | 1922.8 | 2122.8 | 90.2 |
| E | vllm bench serve | 2 | 250 | 0 | 102.3 | 2077.4 | 2242.8 | 1934.8 | 2134.8 | 100.1 |
| F | GuideLLM constant | 2 | 251 | 0 | 96.5 | 2271.8 | 3108.7 | 1921.3 | 2121.3 | 86.9 |
| G | AIPerf constant | 2 | 249 | 0 | 94.8 | 2127.6 | 2346.2 | 1929.2 | 2129.2 | 95.6 |
<!-- END:main_pass2 -->

Metric used per tool (quoted from the tool): oha `latencyPercentiles.p50/p99` and `summary.slowest` ([oha](https://github.com/hatoo/oha)); vLLM `p50_e2el_ms`, `p99_e2el_ms`, `p100_e2el_ms` (E2EL, [vllm bench serve](https://docs.vllm.ai/en/latest/benchmarking/cli/)); GuideLLM `metrics.request_latency.successful` percentiles p50/p99 and max ([GuideLLM](https://github.com/vllm-project/guidellm)); AIPerf `request_latency` p50/p99/max ([AIPerf](https://github.com/ai-dynamo/aiperf)); rs-drive A `open_loop latency = t_done - t_intended`, B `closed_loop latency = t_done - t_sent` ([tailbench](../../README.md)).

Concurrency settings, from each tool's `--help`/docs and recorded config: oha `-q` is "Rate limit for all, in queries per second (QPS)", so 10 is the total rate and not per connection (`oha --help`); `-c` is the number of connections. vLLM bench: `--max-concurrency` default `None` (printed in the run: "Maximum request concurrency: None"). GuideLLM: the recorded strategy config has `max_concurrency 10240`. AIPerf: no `--concurrency` set. rs-drive open: one task per request; closed: at most one in flight.

**How the expected values use the logged freeze.** For every run the phase-averaged expectation takes the logged freeze length (`freeze_s`, STOP to CONT, 2.000-2.005 s) and the logged STOP time relative to the first request the server logged (`freeze_after_first_s`), plus the per-run `s0`. Until the last rebuild it used the logged length but the nominal 8.0 s for the offset; switching to the logged offset changed no expected p99 or max by more than 1.0 ms (the 11 runs whose offset was 9.3-11.0 s included). Config A also uses its exact origin (`expected_exact`). Config E's send times are not available (see next paragraph), so E is compared with the phase-averaged model only.

**vLLM bench arrivals** (`vllm bench serve --help`, in `out/help/vllm-bench.txt`): `--request-rate`: "Number of requests per second. If this is inf, then all the requests are sent at time 0. Otherwise, we use Poisson process or gamma distribution to synthesize the request arrival times." `--burstiness`: "Default value is 1, which follows Poisson process. Otherwise, the request intervals follow a gamma distribution. ... A higher burstiness value (burstiness > 1) results in a more uniform arrival of requests." `--seed` has no help text. E ran with `--burstiness inf`; in the installed source (`vllm/benchmarks/serve.py`, `get_request`) that case is "the delay time becomes constant and tends to the inverse of the request rate", so E's arrivals are evenly spaced at 100 ms (the first at 0.1 s), not Poisson, and the seed does not change the arrival times. The saved `vllm.json` holds per-request latencies and queue times but no send times, so I could not compute an exact expectation: **E is "evenly spaced arrivals, compared against the phase-averaged model only".**

## Results, pass 1

<!-- BEGIN:main_pass1 -->
| cfg | tool | rep | n ok | errors | p50 ms | p99 ms | max ms | expected p99 (phase-avg) | expected max | s0 used ms |
|:--|:--|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| A | tailbench open loop (rs-drive) | 1 | 250 | 0 | 81.2 | 2085.3 | 2229.7 | 1919.1 | 2119.1 | 86.2 |
| B | tailbench closed loop (rs-drive) | 1 | 250 | 0 | 80.2 | 108.9 | 2149.7 | 86.2 | 2168.5 | 86.2 |
| C | oha -c 1 | 1 | 250 | 1 deadline | 81.6 | 130.3 | 2087.6 | 86.2 | 2169.6 | 86.2 |
| D | oha -c 1 --latency-correction | 1 | 250 | 0 deadline | 285.9 | 2210.7 | 2225.5 | 2140.8 | 2168.4 | 86.2 |
| C2 | oha -c 50 | 1 | 230 | 20 conn + 2 deadline | 86.1 | 107.5 | 187.3 | 1921.0 | 2121.0 | 86.2 |
| D2 | oha -c 50 --latency-correction | 1 | 230 | 20 conn + 1 deadline | 88.4 | 101.5 | 167.8 | 1918.8 | 2118.8 | 86.2 |
| E | vllm bench serve | 1 | 250 | 0 | 97.5 | 2075.5 | 2283.6 | 1919.4 | 2119.4 | 86.2 |
| F | GuideLLM constant | 1 | 251 | 0 | 94.1 | 2194.5 | 2382.3 | 1920.0 | 2120.0 | 86.2 |
| G | AIPerf constant | 1 | 249 | 0 | 96.1 | 2075.0 | 2222.7 | 1919.2 | 2119.2 | 86.2 |
| A | tailbench open loop (rs-drive) | 2 | 250 | 1 | 96.3 | 1955.7 | 2245.7 | 1920.4 | 2120.4 | 86.2 |
| B | tailbench closed loop (rs-drive) | 2 | 250 | 0 | 95.8 | 134.8 | 2130.6 | 86.2 | 2169.4 | 86.2 |
| C | oha -c 1 | 2 | 230 | 1 deadline | 98.7 | 122.1 | 2156.0 | 86.2 | 2168.5 | 86.2 |
| D | oha -c 1 --latency-correction | 2 | 230 | 1 deadline | 2009.4 | 2330.7 | 2334.9 | 2141.0 | 2168.7 | 86.2 |
| C2 | oha -c 50 | 2 | 230 | 19 conn + 5 deadline | 105.9 | 180.6 | 2168.6 | 1919.8 | 2119.8 | 86.2 |
| D2 | oha -c 50 --latency-correction | 2 | 230 | 19 conn + 3 deadline | 105.0 | 173.8 | 2107.1 | 1919.0 | 2119.0 | 86.2 |
| E | vllm bench serve | 2 | 250 | 0 | 105.7 | 2104.4 | 2267.3 | 1918.9 | 2118.9 | 86.2 |
| F | GuideLLM constant | 2 | 250 | 0 | 96.6 | 2194.6 | 2294.5 | 1919.0 | 2119.0 | 86.2 |
| G | AIPerf constant | 2 | 249 | 0 | 107.3 | 2157.5 | 2363.7 | 1919.3 | 2119.3 | 86.2 |
<!-- END:main_pass1 -->

## Further observations (from the tables)

**Unexplained:**

- **D's 4130 ms max (pass 2, rep 2).** The FIFO intended-time model expects a max of 2171.5 ms for that run; oha reported 4129.8 ms (p99 2794.0 ms). I did not isolate the cause.

**Other observations:**

- **Open-loop, uncapped tools (A, E, F, G)**: p99 is 2037-2272 ms in pass 2 and 1956-2195 ms in pass 1, above the phase-averaged expected 1920-1936 ms by 109-350 ms; the max is 2231-3109 ms. Every one of these runs puts the freeze in its p99.
- **Single connection, uncorrected (B, C)**: p99 is 105-173 ms in pass 2, within about 80 ms of the expected FIFO p99 (94-101 ms), and not near 2000 ms; the max (2093-2214 ms) shows the freeze. The freeze is in the max and not in the p99 for these rows.
- **oha `-c 1 --latency-correction` (D)**: p50 is 2050 / 2007 ms in pass 2 (286 and 2009 ms in pass 1), p99 2261-2794 ms, max up to 4130 ms. The FIFO model expects a max near 2180 ms; rep 2 reports 4130 ms. With `s0` near 100 ms the backlog does not drain, and I did not isolate how much of the extra 2 s that explains.
- **oha `-c 50` (C2, D2)**: in 7 of 8 runs over both passes oha counted about 19-20 `connection error` entries per run; the p99 it reports (101-183 ms) sits near baseline, and the expected p99 is about 1920-1933 ms. One run (pass 2, C2 rep 2) had no connection errors and reported p99 2101 ms. Details next.
- **Between repetitions** the p99 differs by 7 ms (E), 17 ms (G), 69 ms (F) and 70 ms (A) in pass 2, and in the oha rows by 58 ms (C), 0.5 s (D), 1.9 s (C2: rep 1 had 19 connection errors, rep 2 had none) and 23 ms (D2, which had 19 and 20). The 10-25% shifts of `s0` over the session are of the same order as the gaps between tools in the open-loop group, so I do not rank those tools against each other.

## oha `-c 50`: the connection errors

What oha wrote (pass 2, C2 rep 1, from `out/runs/C2-rep1/stdout.txt`), verbatim:

```
"statusCodeDistribution": { "200": 230 },
"errorDistribution": { "aborted due to deadline": 3, "connection error": 19 }
"successRate": 0.9236947791164659
```

So oha counted 230 requests in its latency statistics and 22 as errors (19 + 3); 249 non-deadline requests were attempted. The freeze covered 20 schedule slots. Per run (all 8 `-c 50` runs, both passes):

<!-- BEGIN:c50 -->
| pass | run | oha errorDistribution (verbatim) | ok | errors / intended | oha p99 ms | expected p99 ms | oha p99.9 ms | oha max ms |
|:--|:--|:--|--:|--:|--:|--:|--:|--:|
| pass 1 | C2 rep 1 | `{"aborted due to deadline": 2, "connection error": 20}` | 230 | 20 / 20 | 107.5 | 1921.0 | 187.3 | 187.3 |
| pass 1 | D2 rep 1 | `{"aborted due to deadline": 1, "connection error": 20}` | 230 | 20 / 20 | 101.5 | 1918.8 | 167.8 | 167.8 |
| pass 1 | C2 rep 2 | `{"aborted due to deadline": 5, "connection error": 19}` | 230 | 19 / 20 | 180.6 | 1919.8 | 2168.6 | 2168.6 |
| pass 1 | D2 rep 2 | `{"aborted due to deadline": 3, "connection error": 19}` | 230 | 19 / 20 | 173.8 | 1919.0 | 2107.1 | 2107.1 |
| pass 2 | C2 rep 1 | `{"aborted due to deadline": 3, "connection error": 19}` | 230 | 19 / 20 | 178.2 | 1931.9 | 2111.1 | 2111.1 |
| pass 2 | D2 rep 1 | `{"aborted due to deadline": 3, "connection error": 19}` | 230 | 19 / 20 | 182.7 | 1930.3 | 2171.2 | 2171.2 |
| pass 2 | C2 rep 2 | `{"aborted due to deadline": 3}` | 249 | 0 / 20 | 2100.9 | 1922.7 | 2265.8 | 2265.8 |
| pass 2 | D2 rep 2 | `{"aborted due to deadline": 3, "connection error": 20}` | 229 | 20 / 20 | 159.2 | 1922.8 | 2095.5 | 2095.5 |
<!-- END:c50 -->

(`errors / intended` counts `connection error` entries against the schedule slots inside the freeze, from the tool's start time; the slot count is 20 in every run, and I do not know oha's exact origin to better than about one slot.)

**How oha treats errors.** I found no prose in oha's README or `--help` that says how errored requests count in the latency percentiles. The only wording is in the source, so this is the source, not documentation: oha 1.16.0, tag [v1.16.0](https://github.com/hatoo/oha/tree/v1.16.0): [`src/result_data.rs` L42-52](https://github.com/hatoo/oha/blob/v1.16.0/src/result_data.rs#L42-L52) (`percentile_iter`), [L55-61](https://github.com/hatoo/oha/blob/v1.16.0/src/result_data.rs#L55-L61) (`push`), [L85-93](https://github.com/hatoo/oha/blob/v1.16.0/src/result_data.rs#L85-L93) (`success_rate`), and the `--latency-correction` help text at [`src/lib.rs` L151](https://github.com/hatoo/oha/blob/v1.16.0/src/lib.rs#L151). `curl -sI` on these URLs returns HTTP 200, and the tag's `result_data.rs` is byte-identical to the installed crate's: `push()` adds an `Ok` result to `success` and an `Err` only to `error_distribution`; the latency percentiles (`percentile_iter`) and `summary.slowest` are computed from `success` only, so errored requests are not in them. `success_rate()` says "We ignore deadline errors which are because of `-z` option". `--latency-correction` is documented as "Correct latency to avoid coordinated omission problem. It's ignored if -q is not set." It does not add a latency for a request that errored. Observed, independent of the source: the errored requests are absent from the latency stats. In the JSON above `statusCodeDistribution` totals 230 (`"200": 230`) and the latency percentiles come from those 230; the 19 `connection error` and 3 `aborted due to deadline` entries appear only in `errorDistribution`. So for the 19 in-freeze requests oha reports no latency at all, and its p99 (178.2 ms) is computed over the other 230. The expected p99 is 1932.8 ms.

**What the text means**: "connection error" is `hyper::Error`'s display for its I/O kind (hyper `src/error.rs`: `Kind::Io => "connection error"`). oha prints only that text, so the underlying `io::Error` is not in its output. I had no `strace`/`tcpdump` or root here, so I did not capture it.

**What I measured** (`diag.py oha`, a one-off with the same command, `ss -tn` every 100 ms, `/proc/net` counters before and after, and the server log; output in `out/diag-oha/`):

- It reproduces: two more freeze runs gave 19 and 18 `connection error`; a no-freeze run gave 0 (so 3 diagnostic runs, plus the 8 main runs above).
- Timeline (freeze run a): 50 connections `ESTAB` on both sides. During the freeze the server-side receive queue (`Recv-Q`) is non-zero on one more socket every 100 ms, reaching 19 (each in-freeze request went out on a different idle connection and sat unread). In the sample taken at CONT, `ESTAB` drops from 50 to 31 on both sides, and stays at 31. So 19 connections ended at resume.
- Counters over the run: `Tcp.EstabResets +38` (2 per connection), `Tcp.OutRsts +19`, `TcpExt.TCPAbortOnClose +19` (run b: 36, 18, 18). In the no-freeze run all three are 0.
- Server log: no ERROR or WARNING lines. The server logged 235 `POST` lines in run a = oha's 230 + 5 plain follow-up requests of the orchestrator, so the 19 errored requests never completed on the server.
- Side: all non-empty receive queues were on the server side, and `TCPAbortOnClose` counts sockets closed with unread data. The counters are host-wide and both ends are on this host, so the counters alone do not say which end closed. The queue evidence points at the server process closing 19 connections at resume, before it read their requests. It is an inference from those two measurements.
- Why the server closed them I did not observe directly. vLLM's definition (`vllm/envs.py` in the installed venv, line 1142-1145): `# Timeout in seconds for keeping HTTP connections alive in API server` / `"VLLM_HTTP_TIMEOUT_KEEP_ALIVE": lambda: int(os.environ.get("VLLM_HTTP_TIMEOUT_KEEP_ALIVE", "5"))`, and `VLLM_HTTP_TIMEOUT_KEEP_ALIVE: int = 5  # seconds`. The hypothesis is that uvicorn closes connections whose 5 s timer is overdue at resume and resets the unread requests on them. The first control (same diagnostic, only `VLLM_HTTP_TIMEOUT_KEEP_ALIVE=300`) gave 0 `connection error` in 2 freeze runs with 50 connections `ESTAB` through the resume; the main runs used the default. The follow-up test below did not confirm the hypothesis cleanly.

### Open question: keep-alive and connection count (`diag_ka.py`)

oha `-q 10 -z 25s --latency-correction` with the same 2.0 s freeze, one run each, one default-keep-alive server for the first five runs (health check and 5 s gap before each; no restart was needed), then a second server started with `VLLM_HTTP_TIMEOUT_KEEP_ALIVE=300` (an environment variable cannot change without a restart). The last default-server run is a normal rs-drive open-loop run, there to read the GPU log. `nvidia-smi` logged clocks, temperature and power every 500 ms throughout; the GPU columns are the log's samples during that run.

<!-- BEGIN:ka -->
| run | server keep-alive | errorDistribution (verbatim) | conn. errors | server ESTAB before STOP / 0.5 s after CONT | EstabResets / OutRsts / TCPAbortOnClose | oha p99 ms | s0 before / after ms | GPU sm MHz mean (min-max) | GPU temp max C | GPU power mean W |
|:--|:--|:--|--:|:--|:--|--:|:--|:--|--:|--:|
| c20 (oha -c 20 --latency-correction) | 5 s (default) | `{"aborted due to deadline": 2}` | 0 | 20 / 20 | 0 / 0 / 0 | 2084.3 | 101 / 102 | 1853 (735-2580) | 89 | 50.1 |
| c100 (oha -c 100 --latency-correction) | 5 s (default) | `{"aborted due to deadline": 4}` | 0 | 51 / 51 | 0 / 0 / 0 | 2509.2 | 106 / 123 | 1649 (420-2355) | 89 | 34.9 |
| c50-a (oha -c 50 --latency-correction) | 5 s (default) | `{"aborted due to deadline": 5}` | 0 | 50 / 50 | 0 / 45 / 0 | 4204.6 | 118 / 226 | 1581 (315-2460) | 90 | 28.4 |
| c50-b (oha -c 50 --latency-correction) | 5 s (default) | `{"aborted due to deadline": 36, "connection error": 2}` | 2 | 50 / 48 | 4 / 2 / 2 | 4155.9 | 103 / 363 | 1432 (210-2580) | 88 | 29.0 |
| normal-A (tailbench open loop (rs-drive)) | 5 s (default) | - | - | 24 / 31 | 0 / 0 / 0 | 3682.9 | 105 / 158 | 1412 (210-2385) | 88 | 24.4 |
| c50-ka300 (oha -c 50 --latency-correction) | 300 s | `{"aborted due to deadline": 5}` | 0 | 50 / 50 | 0 / 0 / 0 | 3179.6 | 104 / 114 | 1509 (360-2400) | 89 | 27.4 |
<!-- END:ka -->

What the data shows:

- **Summary: an open question, with no causal claim.** Main passes: 9 of 10 default-setting `-c 50` freeze runs (8 main, 2 diagnostic) had 18-20 `connection error` each, `ESTAB` fell at resume and kernel resets were counted at that moment, and the server log had no ERROR or WARNING lines. Hypothesis tested: uvicorn closes connections whose 5 s keep-alive timer is overdue at resume and the unread requests on them are reset. Diagnostic results: 0, 0, 0 and 2 `connection error` at the default setting and 0 at 300 s. They do not discriminate, because 3 of the 4 default-setting runs also had 0, and the GPU was thermally throttling (`s0` 102-363 ms after each run, see Thermals), so these runs are not comparable with the main passes. Details:
- **What the test shows.** At the default keep-alive, 3 of the 4 oha runs (`-c 20`, `-c 100`, `-c 50` first run) had 0 `connection error`; the second `-c 50` run had 2, with `EstabResets 4`, `OutRsts 2`, `TCPAbortOnClose 2` and server `ESTAB` 50 to 48 after CONT, which is the same signature as the 19-error runs at smaller scale. With keep-alive 300 the one `-c 50` run had 0 (so 3 runs at 300 s in total, with the two earlier ones, all with 0). But 3 of 4 default runs also had 0, so this test does not separate the two settings. Of the 10 earlier default-setting `-c 50` freeze runs (8 main, 2 diagnostic), 9 had 18-20 `connection error` and 1 had 0; this session's default-setting runs had 0, 0, 0 and 2.
- `-c 20` and `-c 100` did not behave differently from `-c 50` in a way the data can attribute: 0 errors each, `Recv-Q` non-zero on 18 and 19 server sockets at the end of the freeze. I did not test whether connection reuse interval (about 2 s at `-c 20`, 10 s at `-c 100`) matters.
- In this test the server was much slower than in the main runs (s0 after a run 102-363 ms, oha p50 105-1095 ms) and oha reported 2-36 `aborted due to deadline` per run. A slower server changes which connections are idle at the freeze, so the runs are not comparable with the earlier ones. I do not know why the earlier runs reproduced and these did not.
- `c50-a` had `OutRsts 45` and `TCPAbortOnData 7` with no `connection error`; I did not attribute them.

The soak (`diag.py soak`) contained no oha runs, so it adds nothing here.

## oha `-c 1`: request counts

oha schedules a tick every 100 ms into an unbounded queue (source: `work_until_with_qps` sends `()` at `start + i/qps` into `kanal::unbounded()`), and the single connection takes ticks in order, so ticks that arrive during a stall wait and are served late; a request still in flight at the 25 s deadline is counted as `aborted due to deadline` (1 in each of these runs). Counts, against 250 scheduled and the schedule slots that fall inside the freeze (20 in every run):

<!-- BEGIN:counts -->
| pass | run | n ok | shortfall vs 250 | freeze arrivals | shortfall = arrivals? | Σ latency s | FIFO sim n ok |
|:--|:--|--:|--:|--:|:--|--:|--:|
| pass 1 | C rep 1 | 250 | 0 | 20 | no (-20) | 22.7 | 249 |
| pass 1 | D rep 1 | 250 | 0 | 20 | no (-20) | 169.5 | 249 |
| pass 1 | C rep 2 | 230 | 20 | 20 | yes | 24.6 | 249 |
| pass 1 | D rep 2 | 230 | 20 | 20 | yes | 330.3 | 249 |
| pass 2 | C rep 1 | 227 | 23 | 20 | no (+3) | 24.5 | 226 |
| pass 2 | D rep 1 | 227 | 23 | 20 | no (+3) | 319.1 | 234 |
| pass 2 | C rep 2 | 218 | 32 | 20 | no (+12) | 23.8 | 238 |
| pass 2 | D rep 2 | 208 | 42 | 20 | no (+22) | 283.2 | 249 |
<!-- END:counts -->

The shortfall equals the freeze-window arrivals in 2 of 8 runs (pass 1, rep 2, both C and D). In pass 1 rep 1 oha counted all 250. In pass 2 the shortfalls are 23, 23, 32 and 42. `Σ latency` is the count times oha's mean latency: the connection was busy for 22.7-24.6 s of the 25 s in every C run, so the count is limited by how many queued ticks the connection got through before the deadline, which depends on `s0` and on the backlog left after the freeze. A FIFO simulation of that with the measured `s0` (last column; `expected.py` model, actual freeze times) is within 1 of oha's count in 3 runs (250/249, 250/249, 227/226) and off by 7 to 41 in the other 5. I do not have a better model than that.

## vLLM bench dev run: 30 requests in 1.19 s

The run (shell history is unavailable; the command comes from the earlier Claude Code session transcript, 2026-10-25 about 21:15 UTC), with the environment cleared (`env -i HOME=$H PATH=...`):

```
vllm bench serve --backend openai --base-url http://localhost:18000 --endpoint /v1/completions \
  --model Qwen/Qwen2.5-0.5B-Instruct --tokenizer Qwen/Qwen2.5-0.5B-Instruct --dataset-name random \
  --random-input-len 64 --random-output-len 16 --ignore-eos --num-prompts 30 --request-rate 10 \
  --burstiness inf --seed 42 --request-id-prefix '' --percentile-metrics e2el \
  --metric-percentiles 50,90,99,100 --save-result --result-dir . --result-filename vllm.json --disable-tqdm
```

It printed `Benchmark duration (s): 1.19`, `Request throughput 25.19`, `Peak concurrent requests: 30`. At 10 req/s 30 requests should span 2.9 s (the source builds an absolute schedule of `1/rate` steps, normalised to `num_prompts / request_rate`).

**Flags versus the real runs (config E):** identical except `--num-prompts` (30 vs 250), the host (`localhost` vs `127.0.0.1`) and the result directory. So no flag differed in a way that sets the pace. I tested that directly against the repo's fake server with the dev flags: 3 runs with `localhost`, 2 with `127.0.0.1`, 2 with none of the extra flags, 30 prompts each, all took 3.17 s with the server seeing arrivals spaced 97-103 ms, and 2 runs of 250 prompts took 25.2 s. Two earlier dev runs in the same session gave 3.17 s (fake server) and 3.08 s (real server).

**The real runs show the same thing in part:** E took 22.83 s and 22.68 s in pass 1 and 22.77 s and 25.13 s in pass 2, against a 25.0 s schedule. Separately, the orchestrator's `freeze_after_first_s` (wall-clock STOP minus first-POST time, which should be 8.0 s) was 9.3-11.0 s in 11 of the 36 main runs (pass 1: B rep 1, C rep 1, D rep 1, F rep 1, C rep 2, D rep 2; pass 2: A rep 1, D2 rep 1, F rep 1, B rep 2, F rep 2). Both are 2-3 s off in the same direction. A 2-3 s step of the wall clock against the monotonic clock would explain both, but I did not observe one.

**What I could not reproduce:** I ran a 5 ms wall-versus-monotonic clock monitor (it logs any change above 5 ms) during 8 real-server runs (1 + 7) and 9 fake-server runs. It recorded no step. The 5 real-server E runs took 25.12-26.29 s, none below 25 s, and all 12 diagnostic freeze runs (8 here, 4 in the oha diagnostics) had 8.000-8.001 s between first POST and STOP. So the cause of the 1.19 s run, of the 22.7-22.8 s E runs and of the 9.3-11.0 s freeze offsets is undetermined. The E p99/max values do not depend on this in an obvious way (the E p99 are within 10 ms of each other across the shorter and longer runs of pass 2), but I did not prove that.

## Thermals

In the six runs of the keep-alive test (table above), mean SM clock fell from 1853 MHz (`c20`) to 1649, 1581, 1432 and 1412 MHz (the normal rs-drive run), and was 1509 MHz in the last run; the lowest sample in each run was 210-735 MHz. Temperature peaked at 88-90 C throughout and mean power fell from 50 W (`c20`) to 35 W (`c100`) and 24-29 W in the later runs. The `nvidia-smi` clock event reason bits seen were `0x1` (GPU idle), `0x4` (software power cap) and `0x20` (software thermal slowdown), plus `0x24` and `0x400` in some samples; `0x20` appeared in every run. Over the same runs the tool-independent `s0` after each run was 102, 123, 226, 363, 158 and 114 ms. So `s0` rose while the mean clock fell over the first four runs, then `s0` came back down with the clock at 1412-1509 MHz; with six runs and a shared sample window this is an observation of co-movement, not a measured cause. I did not log clocks during the two main passes. 

The main passes did not log the GPU, so the `s0` drift between and within them (91.4 to 85.5 ms at start, 86.9 to 99.8 ms median before and after a run) is **unexplained**; the diagnostic session suggests clocks as a candidate, nothing more. The headline conclusions do not depend on it: they rest on ratios of about 15x between configurations (2037-2107 ms for A against 105-170 ms for B in pass 2), and a 10-25% change in `s0` moves the tool-independent expected values by tens of ms, not by a factor of 15.

## WSL notes

The server's `PATH` is set explicitly in `run.py` (`$HOME/audit-tools/vllm-venv/bin:/usr/local/cuda/bin:/usr/bin:/bin`, `CUDA_HOME=/usr/local/cuda`, no `/mnt/*`). Two earlier attempts to start vLLM (`out/server-attempt1.log`, `out/server-attempt2.log`, both 2026-10-24) logged `FlashInfer kernels are disabled: ... nvcc ... or ninja (PATH) is missing` followed by `EngineCore failed to start`. In those sessions `nvcc` and `ninja` resolved to Windows entries under `/mnt/c`; the explicit `PATH` fixed it. The logs do not record what differed between the two attempts.

## What this does not show

- One GPU, one small model, one freeze length (2.0 s), one rate (10 req/s), 2 repetitions per pass. Variation between repetitions is of the same order as some of the differences discussed.
- `SIGSTOP` of the server's process group is a stand-in for a CPU throttle, a VM pause, a swap storm, preemption or a GC pause. It stops the server completely and leaves the network stack running, which is why the kernel accepts the in-freeze requests (the sockets showed unread data in `Recv-Q`). Other stalls may behave differently.
- The expected values are an approximation (periodic schedule, an `s0` measured with plain requests, one resume penalty `r = 82.3 ms` from three gate freezes). `s0` drifted by 10-25% during the session.
- The `-c 50` and `-c 1` explanations rest on the diagnostic runs above and on oha 1.16.0's source; they are not tested against other oha versions or other servers.
- Tools were run with the flags in `run.py` and may offer settings I did not try (for example `VLLM_HTTP_TIMEOUT_KEEP_ALIVE=300`, which I ran only 3 times; see the open question).
