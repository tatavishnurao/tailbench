# phantom

A deterministic fake OpenAI-compatible server that logs ground truth for every request.

phantom follows a script fixed at startup and writes what it did for each request to a JSONL log, so a harness's measurements can be compared request by request with the truth.

## See it

```sh
phantom demo
```

In about 12 s, in-process: a naive client (one request at a time, timed from its actual send) and then tailbench's open loop, each against a fresh phantom that freezes for 500 ms, both asked for 100 requests/s. Each panel sets the tool's own p99 against what its users waited, on one scale; the summary repeats the same numbers. Its final output:

```text
phantom · 1/2 naive client                                             t = 6.0 s

requests arriving  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▂▁▁▁▁███▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄
server frozen      ····················█████···································

p99 latency
  your tool reports  ▌                                                   4.58 ms
  users waited       ███████████████████████████████████████████████████  459 ms

✗ Your load tool stopped sending while the server was frozen. It will
  under-report p99 by 100×.
your tool's own p99; users waited: truth from phantom's log at 100 req/s
phantom: stopped at t = 6.0 s: 600 answered, 0 abandoned

phantom · 2/2 tailbench open loop                                      t = 6.0 s

requests arriving  ███████████████████████████████████████████████████████████▇
server frozen      ····················█████···································

p99 latency
  your tool reports  ████████████████████████████████████████████████████ 438 ms
  users waited       ███████████████████████████████████████████████████▉ 437 ms

✓ Your load tool kept sending. Its numbers include the freeze.
your tool's own p99; users waited: truth from phantom's log at 100 req/s
phantom: stopped at t = 6.0 s: 600 answered, 0 abandoned

100 req/s for 6 s against 1 ms service, server frozen 2.0–2.5 s:

  p99                      your tool reports  users waited
  1/2 naive client                   4.58 ms        459 ms  ✗ under by 100×
  2/2 tailbench open loop             438 ms        437 ms  ✓ matches the truth

logs: /tmp/phantom-demo-73177/naive.jsonl
      /tmp/phantom-demo-73177/tailbench.jsonl
next: phantom report --rate 100 /tmp/phantom-demo-73177/naive.jsonl --html out.html
```

To watch another load generator, serve with `--live` and point it at phantom (a fresh phantom for each run):

```sh
phantom --live --listen 127.0.0.1:8090 --service-ms 1 --stall 2000:500 --log /tmp/gt-c1.jsonl
oha -q 100 -z 10s -c 1 -m POST -H 'content-type: application/json' -d '{}' http://127.0.0.1:8090/v1/chat/completions
```

Here phantom cannot see the tool's own numbers, so "your tool ≈ reports" is its estimate: the p99 of server time, which is what a tool timing from its actual sends measures. The panel is at most 80 columns wide, on any terminal. The final panels after Ctrl-C:

`-c 1`

```text
phantom 0.2.0 listening on 127.0.0.1:8090, logging to /tmp/gt-c1.jsonl

phantom · watching 127.0.0.1:8090                                     t = 10.5 s

requests arriving  ▄▄▄▄▄▄▄▄▄▄▂▁▅█▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▁▁▁
server frozen      ··········███········································

p99 latency
  your tool ≈ reports  ▍                                                 3.73 ms
  users waited         █████████████████████████████████████████████████  434 ms

✗ Your load tool stopped sending while the server was frozen. It will
  under-report p99 by 116×.
assumes 100 req/s intended, the rate measured outside the freeze
phantom: stopped at t = 10.5 s: 1000 answered, 0 abandoned
```

`-c 50` (logging to `/tmp/gt-c50.jsonl`)

```text
phantom 0.2.0 listening on 127.0.0.1:8090, logging to /tmp/gt-c50.jsonl

phantom · watching 127.0.0.1:8090                                     t = 10.5 s

requests arriving  ███▇██████████████████████████████████████████████▁▁▁
server frozen      ··········███········································

p99 latency
  your tool ≈ reports  █████████████████████████████████████████████████▉ 394 ms
  users waited         ██████████████████████████████████████████████████ 395 ms

✓ Your load tool kept sending. Its numbers include the freeze.
assumes 100 req/s intended, the rate measured outside the freeze
phantom: stopped at t = 10.5 s: 1000 answered, 0 abandoned
```

With one connection oha waits for each reply, so it sends nothing while phantom is frozen and its own p99 leaves the wait out; with fifty it keeps its schedule, and the freeze is in its numbers.

Read a log back, against the rate the client was asked for, as a chart and percentiles, or a self-contained HTML page:

```sh
phantom report --rate 100 /tmp/gt-c1.jsonl --html /tmp/gt-c1.html
```

To see what an app built on the openai Python SDK shows its user when a provider freezes, run the [provider freeze drill](https://github.com/tatavishnurao/tailbench/blob/main/examples/freeze-drill/README.md).

## Install

```sh
cargo install --locked tailbench-phantom
```

From source, in a checkout of the [tailbench repository](https://github.com/tatavishnurao/tailbench):

```sh
cargo install --path phantom
```

## Example

Start phantom with a fixed 20 ms service time and one stall window:

```sh
phantom --listen 127.0.0.1:8080 --seed 1 --service-ms 20 --stall 2000:500 --log /tmp/gt.jsonl
```

In another shell, send two requests, then stop phantom with Ctrl-C:

```sh
curl -s -H 'content-type: application/json' -d '{}' http://127.0.0.1:8080/v1/chat/completions
curl -s -H 'content-type: application/json' -d '{}' http://127.0.0.1:8080/v1/chat/completions
```

`/tmp/gt.jsonl` then holds the manifest and one line per request:

```json
{"type":"manifest","phantom_version":"0.2.0","seed":1,"service":{"kind":"fixed","ms":20},"stalls":[{"start_ms":2000,"len_ms":500}],"ttft":{"kind":"fixed","ms":20},"itl":{"kind":"fixed","ms":10},"output_tokens":32,"role_chunk":"with-first-token","token_text":" tok","listen":"127.0.0.1:8080","start_unix_ns":1792675346756144385,"times":"ns since t0, the arrival of the first chat or completions request, on the server's monotonic clock"}
{"type":"req","seq":0,"arrival_ns":148,"scripted_service_ns":20000000,"stall_wait_ns":0,"sent_ns":22616234}
{"type":"req","seq":1,"arrival_ns":29606223,"scripted_service_ns":20000000,"stall_wait_ns":0,"sent_ns":52109987}
```

Every field is described in the [truth log reference](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/truth-log.md).

## Audit findings

Headline findings, quoted from the audit reports:

| audit | finding | report |
|:---|:---|:---|
| tailbench | In open loop, tailbench reads higher than truth in every cell, by 0.65 to 3.51 ms. | [tailbench.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/tailbench.md#audit-results) |
| tailbench | In C, tailbench's closed-loop p99 was 1.07–1.15% of B's truth p99: that is coordinated omission, which the closed-loop runner has by design. | [tailbench.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/tailbench.md#audit-results) |
| oha 1.16.0 | Read at oha's own index, oha's p50, p99, p99.9 and max are within 0.81 ms of this run's truth in every scenario except O4 p99. | [oha.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/oha.md#results) |
| oha 1.16.0 | At `-c 1`, uncorrected (O4), the requests that could not be sent during the stall are measured from when they were actually sent, so p99 reads 4.2–4.5 ms against this run's truth of 431–439 ms (coordinated omission). | [oha.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/oha.md#candidate-explanations) |
| GuideLLM 0.8.1 | GuideLLM − server TTFT was +0.29 to +0.93 ms in every cell of every scenario. | [guidellm.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/guidellm.md#results) |
| GuideLLM 0.8.1 | In G2 and G3, the stalled request's mean was about 84 ms, while the largest pooled gap was 2.00–2.02 s. | [guidellm.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/guidellm.md#results) |
| GuideLLM 0.8.1 | GuideLLM `request_scheduled_latency` − intended truth was +1.36 to +2.48 ms in G1, G2 and G4. | [guidellm.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/guidellm.md#results) |
| vLLM bench serve 0.32.0 | At numpy's method, vLLM − server TTFT was +1.08 to +1.64 ms in V1, +1.02 to +1.99 ms in V2, +0.87 to +1.28 ms in V3 and +0.99 to +1.65 ms in V5: +0.87 to +1.99 ms over those four. | [vllm.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/vllm.md#results) |
| vLLM bench serve 0.32.0 | In V2 to V5, the maximum ITL was the stall itself, 2.01–2.02 s. | [vllm.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/vllm.md#results) |
| vLLM bench serve 0.32.0 | For every request in every run, vLLM's TTFT plus its client_queue_time was within −1.35 to +1.42 ms of the same request's intended TTFT (p50 +0.25 to +0.56 ms). | [vllm.md](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/vllm.md#results) |

The tools' definitions of TTFT, ITL, TPOT and percentiles are compared in the [audits index](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/README.md#oha-guidellm-and-vllm-compared).

## Documentation

- [Endpoints, streaming and CLI flags](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/endpoints.md): flags, endpoints, the event stream and its vLLM-compatible ending.
- [Script and token schedule](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/spec.md): seq, service time, stalls and the absolute token schedule.
- [Truth log](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/truth-log.md): the JSONL schema, abandoned requests, `client_id`, and [LIMITS](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/truth-log.md#limits).
- [Audits](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/README.md): method index and the cross-tool definitions.
  - [tailbench](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/tailbench.md)
  - [oha](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/oha.md)
  - [GuideLLM](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/guidellm.md)
  - [vLLM bench serve](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/audits/vllm.md)

## License

Apache-2.0; see [LICENSE](https://github.com/tatavishnurao/tailbench/blob/main/LICENSE).
