# Audit: oha

`tests/audit_oha.rs` runs [oha](https://github.com/hatoo/oha) 1.16.0 against an in-process phantom: fixed 1 ms service, seed 1, stall `2000:500`, with a fresh phantom for each scenario. oha is an external binary, not a dependency, and this is a report, not a pass/fail gate for oha. The only assertions are that oha exits 0, its JSON parses, and phantom logs at least 100 requests.

```sh
cargo install oha --locked   # without --locked, a dependency needs rustc 1.96 (this machine: 1.94.1)
cargo test --release -p tailbench-phantom --test audit_oha -- --ignored --nocapture --test-threads=1
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

## Results

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

## Candidate explanations

Candidate explanations, none of them verified:

- **Connection count.** With 50 connections, the 50 requests stalled in the 500 ms window (500 ms × 100/s) take exactly 50 connections, so arrivals keep flowing (largest gap 11–21 ms; at most 1 of 1000 requests later than one period). With 10 or 1 connection, every connection is held by the stall, and arrivals stop for about 400–505 ms (40–42 late requests at `-c 10`, 73–75 at `-c 1`). Then a burst follows: 45–46 arrivals in the 50 ms after the stall at `-c 10`.
- **Rate limiter semantics.** At `-c 1`, uncorrected (O4), the requests that could not be sent during the stall are measured from when they were actually sent, so p99 reads 4.2–4.5 ms against this run's truth of 431–439 ms (coordinated omission). With `--latency-correction` (O5), p99 at oha's index is within 0.81 ms of this run's truth. How `-q` schedules sends that are already late is oha's own design, and this audit did not examine it.
- **Burst after the stall.** At `-c 10` the backlog arrives at once after the stall, so the response times right after the stall depend on how oha catches up. Requests that arrive together may be permuted relative to send order, which seq-keyed truth cannot detect (see Limits).
- **Phase of the schedule against the stall.** phantom's t0 is oha's first arrival. Whether a request lands at the very start of the window decides whether the longest stall_wait is about 490 or 500 ms. That would move the max and tail cells by about one 10 ms period between runs. It would account for much of the spread in the cross-run column, and it cannot affect the per-run columns, which compare oha with truth from the same run.
- **Requests aborted at the deadline.** These are not in oha's percentiles. phantom logs only requests that reached its handler: an aborted one that did leaves a `"completed": false` line, and truth is computed over completed lines only. In every scenario, phantom's `completed: false` count was at most oha's "aborted due to deadline" count, and equal only at `-c 1`. The difference is requests cancelled before they reached a handler.

## Limits

Limits:

- **The reference is intended-schedule truth from tailbench's own run,** not an independent measurement of oha's run. oha sends no per-request id. Per-run truth matches its requests to log lines by arrival order (`seq`), assuming oha sends in schedule order and arrival order equals send order. Inside a post-stall burst, requests that arrive together may be permuted. That changes individual truth values, but by no more than the burst's spread.
- **Loopback only:** oha, tailbench and phantom share one machine and one kernel.
- **Corrected latency is oha's own definition.** `oha --help` (1.16.0) describes it as: "Correct latency to avoid coordinated omission problem. It's ignored if -q is not set."
- **Release build, one machine.** These numbers come from one machine; other machines and debug builds were not measured.
- **Different percentile conventions.** oha truncates the index and nearest rank rounds it up (see above). Comparing at nearest rank alone would show one rank of difference as a measurement difference.
