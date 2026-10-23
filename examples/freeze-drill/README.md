# Provider freeze drill

What does an app built on the official `openai` Python SDK show its user when the LLM provider freezes for 15 s, and how many requests does it send into the freeze?

## Run it

```sh
cargo build --release -p tailbench-phantom        # from the repository root
python3 -m venv examples/freeze-drill/.venv && examples/freeze-drill/.venv/bin/pip install -r examples/freeze-drill/requirements.txt
examples/freeze-drill/.venv/bin/python examples/freeze-drill/drill.py
```

It takes about a minute and a half (96.0 s and 95.9 s in the two runs below) and writes `drill.json` (every raw measurement), `drill.html` (a timeline per cell and the card as a table; no JavaScript) and, in `out/`, phantom's log of each cell. Nine cells: three freezes times three client configs, each against a fresh phantom.

- **before first token**: streaming, `--stall 100:15000` with `--service-ms 1 --ttft-ms 200 --itl-ms 30 --output-tokens 40`.
- **mid-answer**: the same script with `--stall 600:15000`.
- **non-streaming**: a plain `chat.completions.create` returning the whole answer, `--service-ms 1 --output-tokens 40 --stall 0:15000`. This one starts the freeze at 0, not 100: phantom holds a non-streaming request only if it *arrives* inside the stall window, and the app's first request arrives at t0. With `100:15000` the same call returned the answer at 0.0 s (with `--service-ms 1`) and at 0.2 s (with `--service-ms 200`), one request each, in two trial runs.

## The drill card

```text
python 3.12.3 · openai 3.37.0 · httpx2 2.16.0 (httpx: not installed) · phantom 0.2.0
SDK defaults: timeout=Timeout(connect=5.0, read=600, write=600, pool=600) max_retries=2
phantom script, streaming: --service-ms 1 --ttft-ms 200 --itl-ms 30 --output-tokens 40 + --stall START:15000 (100 or 600)
phantom script, non-streaming: --service-ms 1 --output-tokens 40 + --stall 0:15000

freeze              client        requests  user saw
before first token  SDK defaults  1         nothing for 15.1 s, finished 40/40
                                            create() returned at 0.02 s · phantom log: #0 at 0.0 s completed (40 tok)
before first token  timeout=10    1         nothing for 10.0 s, then error APITimeoutError at 10.0 s
                                            create() returned at 0.00 s · phantom log: #0 at 0.0 s abandoned 10.0 s (0 tok)
before first token  read=2        1         nothing for 2.0 s, then error APITimeoutError at 2.0 s
                                            create() returned at 0.00 s · phantom log: #0 at 0.0 s abandoned 2.0 s (0 tok)
mid-answer          SDK defaults  1         14/40 tokens, then frozen 15.0 s, finished 40/40
                                            create() returned at 0.00 s · phantom log: #0 at 0.0 s completed (40 tok)
mid-answer          timeout=10    1         14/40 tokens, then frozen 10.0 s, then error APITimeoutError at 10.6 s
                                            create() returned at 0.00 s · phantom log: #0 at 0.0 s abandoned 10.6 s (14 tok)
mid-answer          read=2        1         14/40 tokens, then frozen 2.0 s, then error APITimeoutError at 2.6 s
                                            create() returned at 0.00 s · phantom log: #0 at 0.0 s abandoned 2.6 s (14 tok)
non-streaming       SDK defaults  1         nothing for 15.0 s, then the whole answer
                                            phantom log: #0 at 0.0 s completed
non-streaming       timeout=10    2         nothing for 15.0 s, then the whole answer
                                            phantom log: #0 at 0.0 s abandoned 10.0 s; #1 at 10.5 s completed
non-streaming       read=2        3         nothing for 7.4 s, then error APITimeoutError
                                            phantom log: #0 at 0.0 s abandoned 2.0 s; #1 at 2.4 s abandoned 4.4 s; #2 at 5.3 s abandoned 7.3 s
```

Configs: A is `OpenAI(base_url=..., api_key=...)`, B adds `timeout=10`, C is `timeout=openai.Timeout(10, read=2)`. This is openai 3.37.0, which imports `httpx2`, not `httpx`; `openai.Timeout` is the same timeout type. Streaming cells are capped at 40 s and non-streaming at 120 s from the drill's side; none reached its cap. Two runs gave the same story: the same token counts and request counts in every cell, and outcome times within 0.04 s of each other (the retries' arrival times moved by up to 0.1 s between runs).

## What the drill measured

1. **With the SDK's defaults the app showed nothing wrong; it waited out the freeze.** The default timeout is `Timeout(connect=5.0, read=600, write=600, pool=600)`. Before the first token the user saw nothing for 15.1 s; mid-answer they saw 14 of 40 tokens and then nothing for 15.0 s. Both then finished 40/40, at 16.3 s and 16.4 s, with no exception.
2. **A timeout turns the freeze into an exception, at the read timeout after the last thing received.** With `timeout=10` the call raised `APITimeoutError` ("Request timed out.") at 10.0 s before the first token and at 10.6 s mid-answer, where the last token had arrived at 0.59 s. With `read=2` it raised at 2.0 s and at 2.6 s. In the mid-answer cells the 14 tokens had already been received when the exception came.
3. **The six streaming cells each sent exactly one request, with `max_retries` at 2.** The effective client had `max_retries=2` in every cell, and phantom's log shows one request each: completed (A) or abandoned at the client's timeout, after 0 or 14 tokens were sent (B, C). In every streaming cell `create()` returned within 0.02 s, because phantom sends the response headers as soon as the request arrives and holds the stream body; the timeouts fired while the app was reading the stream. One run of the check below shows the headers arriving before the freeze.
4. **The non-streaming cells did retry, into the freeze.** With the same `max_retries=2`: A sent 1 request and the user saw the whole answer at 15.0 s. B sent 2: the first was abandoned at 10.0 s, the second arrived 0.46 s later (10.5 s), was held by the freeze, and completed at 15.0 s, which is when the user saw the whole answer, with no error. C sent 3: they arrived at 0.0, 2.4 and 5.3 s (2.5 and 5.4 s in the second run), each was abandoned 2.0 s after it arrived, and the call raised `APITimeoutError` at 7.4 s with no answer. The second request came 0.4–0.5 s after the first was abandoned and the third 0.9–1.0 s after the second. Every retry arrived inside the freeze.

### The headers check (one run)

A fresh phantom with `--stall 100:15000` and the streaming script, one streaming request, curl stopped after 5 s:

```text
$ curl -sN -i -m 5 -w '\n[curl: %{time_starttransfer}s to first byte]\n' -H 'content-type: application/json' \
    -d '{"model":"phantom","stream":true,"messages":[{"role":"user","content":"hi"}]}' http://127.0.0.1:PORT/v1/chat/completions
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
transfer-encoding: chunked
date: Fri, 23 Oct 2026 06:52:21 GMT


[curl: 0.000167s to first byte]
```

The status line and headers came 0.000167 s after the request, and no body arrived in the 5 s. This is one run; the time varies between runs.

## What to set in your app

Only what the drill showed:

- If you leave the defaults, a frozen provider means your user waits for it: the read timeout is 600 s, so in this drill the call neither raised nor retried, streaming or not.
- To bound how long a stream can go silent, set a read timeout. `openai.Timeout(10, read=2)` made the call raise after 2.0 s of silence, counted from the last token received (2.6 s mid-answer, whose last token was at 0.59 s). The read timer restarts at each token (10.6 s = last token at 0.59 s + 10 s), so choose `read` against the gaps you see when your provider is healthy; the scripted gaps here were 0.03 s between tokens and 0.2 s to the first.
- Plain `timeout=10` raised at 10.0 s before the first token and 10.6 s mid-answer.
- Catch `openai.APITimeoutError` around the stream loop, not only around `create()`: for a stream it was raised from the iteration, after `create()` had returned, and the tokens already received are yours to keep or discard.
- On a non-streaming call a timeout is not the end of the request: with `max_retries=2`, `read=2` sent 3 requests into the freeze over 7.4 s before the error, and `timeout=10` sent 2 and then returned the answer at 15.0 s. The drill did not vary `max_retries`, so it does not show what a lower value would do; if the extra requests matter, that is the setting to try next.

See the openai-python documentation on [timeouts](https://github.com/openai/openai-python#timeouts) and [retries](https://github.com/openai/openai-python#retries).
