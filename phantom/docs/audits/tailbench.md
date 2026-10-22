# Audit: tailbench

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

## Limits

Limits of the audit:

- **Truth cannot see client-side serialization or socket-read time.** sent_ns is taken in the handler before the response is written. tailbench's latency also includes hyper writing the response, the loopback trip back, and reqwest reading the body. Truth is also low by the smallest send-to-handler delay, which the origin absorbs.
- **tokio timers have 1 ms granularity**, on both sides. tailbench's sends wake up late by up to about 2 ms (its scheduler lag), and phantom's 1 ms sleep overshoots. Both are measured, but they limit how small a delta can be resolved.
- **Loopback only.** Client and server share one machine and one kernel. Nothing here measures a real network.
- **Request order is assumed.** Both tailbench runners call the request closure in schedule order, so request index `i` (sent as `x-request-id`) is send `i` and its intended time is `i * period`. Scenario C also assumes B is a fair reference: same script and rate, but a different run.
