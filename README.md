<h1 align="center">tailbench</h1>
<p align="center">Rust latency harness that doesn't hide your p99.</p>
<p align="center">
  [license badge] [stars badge] [rust badge]
</p>

Closed-loop benchmarks wait for each response before sending the next request,
so a server stall hides itself. tailbench sends on a fixed schedule, measures
from the **intended** send time, and records the result in HdrHistogram with
bootstrap confidence intervals and a reproducible run manifest.

## Demo
[terminal recording: 500 ms stall, closed-loop vs open-loop]

## Why this exists
| Measurement | p99 reported |
|---|---|
| Closed-loop | TBD from the real run |
| Open-loop (tailbench) | TBD from the real run |

Same server, same stall. Only the measurement method differs.
Background: Tene, "How NOT to Measure Latency".

## Quickstart
git clone https://github.com/tatavishnurao/tailbench
cd tailbench
cargo run --release --example stall_test

## Use cases
- Checking whether a benchmark you read is hiding tail latency
- Comparing two serving configs with CIs instead of single numbers
- Recording hardware/driver/cache state so results can be re-run

## How it works
[short diagram: scheduler -> intended send time -> server -> histogram -> report]

## Limits
- p99 from n=30 is effectively the maximum; use n >= 200 for headline cells
- Laptop and WSL2 results are noisy; see the manifest for what was recorded
- Not a load-testing replacement for production traffic replay

## Roadmap
KV reload-vs-recompute probe, GPU restore benchmarks

## License
Apache-2.0
