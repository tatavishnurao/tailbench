#!/usr/bin/env python3
"""Provider freeze drill.

What does an app built on the official openai Python SDK show its user when the
LLM provider freezes, and how many requests does it send into the freeze?

Nine cells: 3 freezes x 3 client configs, each against a fresh phantom on a free
127.0.0.1 port. The client side is timed with time.perf_counter; the server
side is read back from phantom's ground-truth log. Writes drill.json and
drill.html next to this file and prints the drill card.

Standard library + openai only. Run it with the venv's python.
"""

import html
import json
import os
import platform
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.request
from importlib import metadata
from pathlib import Path

import openai

HERE = Path(__file__).resolve().parent
PHANTOM = Path(os.environ.get("PHANTOM", HERE.parent.parent / "target" / "release" / "phantom"))
OUT = HERE / "out"

OUTPUT_TOKENS = 40
SCRIPT = ["--service-ms", "1", "--ttft-ms", "200", "--itl-ms", "30", "--output-tokens", str(OUTPUT_TOKENS)]
# A non-streaming answer has no first token or gaps, so it gets no --ttft-ms/--itl-ms.
SCRIPT_PLAIN = ["--service-ms", "1", "--output-tokens", str(OUTPUT_TOKENS)]
# (freeze, stall START:LEN in ms, stream?, phantom flags, cap in s from the drill's side)
FREEZES = [
    ("before first token", "100:15000", True, SCRIPT, 40.0),
    ("mid-answer", "600:15000", True, SCRIPT, 40.0),
    # A non-streaming request is held only if it arrives inside the window (phantom's spec), and the
    # app's only first request arrives at t0 = 0. Measured with 100:15000: answered at 0.0 s (with
    # --service-ms 1) and at 0.2 s (with --service-ms 200), one request each, no freeze. So 0:15000.
    ("non-streaming", "0:15000", False, SCRIPT_PLAIN, 120.0),
]
# A gap this long between two things the user saw counts as "frozen".
FROZEN_GAP_S = 1.0


def make_client(name, base_url):
    kw = dict(base_url=base_url, api_key="x")
    if name == "A":
        return openai.OpenAI(**kw)
    if name == "B":
        return openai.OpenAI(timeout=10, **kw)
    if name == "C":
        return openai.OpenAI(timeout=openai.Timeout(10, read=2), **kw)
    raise ValueError(name)


CONFIGS = [
    ("A", "SDK defaults", "OpenAI(base_url=..., api_key=...)"),
    ("B", "timeout=10", "OpenAI(timeout=10)"),
    ("C", "read=2", "OpenAI(timeout=Timeout(10, read=2))"),
]


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_phantom(flags, stall, log, stderr_path):
    port = free_port()
    cmd = [str(PHANTOM), "--listen", f"127.0.0.1:{port}", *flags, "--stall", stall, "--log", str(log)]
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=open(stderr_path, "wb"))
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"phantom exited early: {cmd}")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1):
                return proc, port, cmd
        except OSError:
            time.sleep(0.05)
    proc.kill()
    raise RuntimeError("phantom did not answer /health")


def stop_phantom(proc):
    """SIGINT, then wait for it to exit so its log is flushed."""
    proc.send_signal(signal.SIGINT)
    try:
        proc.wait(timeout=60)
        return "exited after SIGINT"
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        return "killed after 60 s (log may be incomplete)"


def app_call(client, stream, rec, lock, t_start):
    """One chat completion, streaming or not, as the app would make it."""

    def put(**kw):
        with lock:
            if not rec["capped"]:
                rec.update(kw)

    try:
        resp = client.chat.completions.create(
            model="phantom",
            stream=stream,
            messages=[{"role": "user", "content": "Say something."}],
        )
        if not stream:
            usage = getattr(resp, "usage", None)
            put(answer_tokens=getattr(usage, "completion_tokens", None))
            put(outcome="finished", ended_s=time.perf_counter() - t_start)
            return
        put(create_returned_s=time.perf_counter() - t_start)
        for chunk in resp:
            text = chunk.choices[0].delta.content if chunk.choices else None
            if text:
                with lock:
                    if not rec["capped"]:
                        rec["tokens_s"].append(time.perf_counter() - t_start)
        put(outcome="finished", ended_s=time.perf_counter() - t_start)
    except BaseException as e:  # the app sees whatever the SDK raises
        put(
            outcome="exception",
            error_class=f"{type(e).__module__}.{type(e).__name__}",
            error_message=str(e),
            ended_s=time.perf_counter() - t_start,
        )


def read_log(path):
    reqs = []
    manifest = None
    for line in Path(path).read_text().splitlines():
        row = json.loads(line)
        if row["type"] == "manifest":
            manifest = row
        elif row["type"] == "req":
            reqs.append(row)
    reqs.sort(key=lambda r: r["arrival_ns"])
    out = []
    for r in reqs:
        done = r.get("completed", True)
        out.append(
            {
                "seq": r["seq"],
                "arrival_s": r["arrival_ns"] / 1e9,
                "completed": done,
                "abandoned_s": None if done else r["abandoned_ns"] / 1e9,
                "sent_s": r.get("sent_ns", 0) / 1e9 if done else None,
                "tokens_sent": len(r.get("token_ns", [])),
                "stream": r.get("stream", False),
            }
        )
    return manifest, out


def run_cell(freeze, stall, stream, flags, cap_s, cfg_id, cfg_name, cfg_code, index):
    tag = f"{index}-{cfg_id}"
    log = OUT / f"cell{tag}.jsonl"
    proc, port, cmd = start_phantom(flags, stall, log, OUT / f"cell{tag}.stderr")
    base_url = f"http://127.0.0.1:{port}/v1"
    client = make_client(cfg_id, base_url)
    rec = {"capped": False, "tokens_s": [], "outcome": None, "ended_s": None, "create_returned_s": None}
    lock = threading.Lock()
    t_start = time.perf_counter()
    th = threading.Thread(target=app_call, args=(client, stream, rec, lock, t_start), daemon=True)
    th.start()
    th.join(cap_s)
    with lock:
        if th.is_alive() or rec["outcome"] is None:
            rec["outcome"] = f"still waiting at {cap_s:.0f} s"
            rec["ended_s"] = None
        rec["capped"] = True
        wall = time.perf_counter() - t_start
        snap = {k: (list(v) if isinstance(v, list) else v) for k, v in rec.items() if k != "capped"}
    exit_note = stop_phantom(proc)
    manifest, reqs = read_log(log)
    tokens = snap["tokens_s"]
    return {
        "freeze": freeze,
        "stall": stall,
        "stream": stream,
        "cap_s": cap_s,
        "config": cfg_name,
        "config_code": cfg_code,
        "client_effective": {
            "timeout": repr(client.timeout),
            "max_retries": client.max_retries,
        },
        "phantom_cmd": [os.path.basename(cmd[0]), *cmd[1:]],
        "requests_total": len(reqs),
        "phantom_exit": exit_note,
        "client": {
            "create_returned_s": snap["create_returned_s"],
            "answer_tokens": snap.get("answer_tokens"),
            "time_to_first_token_s": tokens[0] if tokens else None,
            "token_times_s": tokens,
            "tokens_received": len(tokens),
            "outcome": snap["outcome"],
            "error_class": snap.get("error_class"),
            "error_message": snap.get("error_message"),
            "outcome_at_s": snap["ended_s"],
            "wall_s": wall,
        },
        "phantom_log": {"requests": reqs, "stalls": manifest.get("stalls") if manifest else None},
    }


def user_saw(cell):
    c = cell["client"]
    n = c["tokens_received"]
    times = c["token_times_s"]
    end = c["outcome_at_s"] if c["outcome_at_s"] is not None else cell["cap_s"]
    if not cell["stream"]:
        if c["outcome"] == "finished":
            return f"nothing for {end:.1f} s, then the whole answer"
        if c["outcome"] == "exception":
            return f"nothing for {end:.1f} s, then error {c['error_class'].rsplit('.', 1)[-1]}"
        return f"nothing for {end:.0f} s, {c['outcome']}"
    marks = [0.0, *times, end]
    gaps = [(marks[i + 1] - marks[i], i) for i in range(len(marks) - 1)]
    g, at = max(gaps)
    before = at  # tokens seen before the longest gap
    if g < FROZEN_GAP_S:
        head = f"{n}/{OUTPUT_TOKENS} tokens"
    elif before == 0:
        head = f"nothing for {g:.1f} s"
    else:
        head = f"{before}/{OUTPUT_TOKENS} tokens, then frozen {g:.1f} s"
    if c["outcome"] == "finished":
        tail = f"finished {n}/{OUTPUT_TOKENS}" if g >= FROZEN_GAP_S else "finished"
        return f"{head}, {tail}"
    if c["outcome"] == "exception":
        name = c["error_class"].rsplit(".", 1)[-1]
        if g < FROZEN_GAP_S:
            return f"{head}, then error {name}"
        return f"{head}, then error {name} at {end:.1f} s"
    return f"{head}, {c['outcome']}"


def phantom_request(r):
    tok = f" ({r['tokens_sent']} tok)" if r["stream"] else ""
    if r["completed"]:
        return f"#{r['seq']} at {r['arrival_s']:.1f} s completed{tok}"
    return f"#{r['seq']} at {r['arrival_s']:.1f} s abandoned {r['abandoned_s']:.1f} s{tok}"


def phantom_saw(cell):
    reqs = cell["phantom_log"]["requests"]
    ret = cell["client"]["create_returned_s"]
    logged = "; ".join(phantom_request(r) for r in reqs) if reqs else "no requests"
    head = "" if ret is None else f"create() returned at {ret:.2f} s · "
    return f"{head}phantom log: {logged}"


def card(cells):
    rows = []
    for c in cells:
        rows.append(
            (c["freeze"], c["config"], str(len(c["phantom_log"]["requests"])), user_saw(c), phantom_saw(c))
        )
    head = ("freeze", "client", "requests", "user saw")
    w = [max(len(h), *(len(r[i]) for r in rows)) for i, h in enumerate(head)]
    line = lambda r: "  ".join(str(x).ljust(w[i]) for i, x in enumerate(r[:4])).rstrip()
    out = [line(head)]
    for r in rows:
        out.append(line(r))
        out.append(" " * (sum(w[:3]) + 6) + r[4])
    return "\n".join(out)


# ---- drill.html -----------------------------------------------------------

CSS = """
:root { --bg:#fff; --fg:#1f2328; --muted:#59636e; --line:#d1d9e0; --freeze:#ffd8d3; --tok:#0969da;
  --bad:#cf222e; --retry:#8250df; --ok:#1a7f37; --row:#f6f8fa; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { --bg:#0d1117; --fg:#e6edf3;
  --muted:#9198a1; --line:#3d444d; --freeze:#5a1e1e; --tok:#58a6ff; --bad:#ff7b72; --retry:#d2a8ff;
  --ok:#56d364; --row:#161b22; } }
:root[data-theme="dark"] { --bg:#0d1117; --fg:#e6edf3; --muted:#9198a1; --line:#3d444d; --freeze:#5a1e1e;
  --tok:#58a6ff; --bad:#ff7b72; --retry:#d2a8ff; --ok:#56d364; --row:#161b22; }
body { background:var(--bg); color:var(--fg); font:15px/1.5 system-ui,sans-serif; margin:0 auto;
  max-width:1100px; padding:16px; }
h1 { font-size:22px; margin:0 0 4px; }
p.sub { color:var(--muted); margin:0 0 16px; font-size:13px; overflow-wrap:anywhere; }
.chart { overflow-x:auto; border:1px solid var(--line); border-radius:6px; }
svg { display:block; min-width:760px; width:100%; height:auto; }
svg text { fill:var(--fg); font:12px system-ui,sans-serif; }
svg .muted { fill:var(--muted); }
svg .grid { stroke:var(--line); stroke-width:1; }
svg .freeze { fill:var(--freeze); }
svg .tok { stroke:var(--tok); stroke-width:2; }
svg .bad { fill:var(--bad); font-weight:700; font-size:15px; }
svg .retry { fill:var(--retry); font-weight:700; font-size:15px; }
svg .ok { fill:var(--ok); font-weight:700; font-size:15px; }
svg .lab-ok { fill:var(--ok); }
svg .lab-bad { fill:var(--bad); }
.legend { color:var(--muted); font-size:13px; margin:8px 0 16px; }
.table { overflow-x:auto; }
table { border-collapse:collapse; width:100%; font-size:13px; }
th, td { text-align:left; padding:6px 10px; border-bottom:1px solid var(--line); vertical-align:top; }
th { color:var(--muted); font-weight:600; }
td.num { text-align:right; font-variant-numeric:tabular-nums; }
code { font:12px ui-monospace,monospace; overflow-wrap:anywhere; }
"""


def short_outcome(c):
    """The outcome in words, for the label over a timeline row."""
    cl = c["client"]
    n, end = cl["tokens_received"], cl["outcome_at_s"]
    reqs = len(c["phantom_log"]["requests"])
    what = f"{reqs} request{'s' if reqs != 1 else ''}"
    if cl["outcome"] == "finished":
        got = f"{n}/{OUTPUT_TOKENS} tokens" if c["stream"] else "whole answer"
        return f"finished, {got} at {end:.1f} s · {what}"
    if cl["outcome"] == "exception":
        name = cl["error_class"].rsplit(".", 1)[-1]
        got = f"{n}/{OUTPUT_TOKENS} tokens, " if c["stream"] else ""
        return f"{name}, {got}at {end:.1f} s · {what}"
    return f"{cl['outcome']} · {what}"


def render_html(meta, cells):
    e = html.escape
    # One shared scale: 0 to 40 s, or further if any cell ran longer.
    seen = [c["client"]["outcome_at_s"] or c["cap_s"] for c in cells]
    seen += [r["arrival_s"] for c in cells for r in c["phantom_log"]["requests"]]
    axis = max(40, -(-int(max(seen) + 1) // 5) * 5)
    left, right, top, row_h = 16, 16, 34, 74
    plot_w = 900
    width = left + plot_w + right
    height = top + row_h * len(cells) + 8
    x = lambda s: left + min(max(s, 0), axis) / axis * plot_w
    svg = [
        f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="timeline of each drill cell, 0 to {axis} seconds">'
    ]
    for t in range(0, axis + 1, 5):
        svg.append(f'<line class="grid" x1="{x(t):.1f}" y1="{top - 6}" x2="{x(t):.1f}" y2="{height - 8}"/>')
        svg.append(f'<text class="muted" x="{x(t):.1f}" y="{top - 12}" text-anchor="middle">{t} s</text>')
    for i, c in enumerate(cells):
        y0 = top + i * row_h
        bar_top, bar_h = y0 + 40, 26
        mid = bar_top + bar_h / 2
        start_s, len_s = (float(v) / 1000 for v in c["stall"].split(":"))
        svg.append(
            f'<rect class="freeze" x="{x(start_s):.1f}" y="{bar_top}" width="{x(start_s + len_s) - x(start_s):.1f}" '
            f'height="{bar_h}"/>'
        )
        svg.append(f'<line class="grid" x1="{left}" y1="{y0 + row_h - 2}" x2="{left + plot_w}" y2="{y0 + row_h - 2}"/>')
        # The label sits over the row, at the left, so no width can cut it off.
        svg.append(
            f'<text x="{left}" y="{y0 + 16}" font-weight="600">{e(c["freeze"])} · {e(c["config"])}</text>'
        )
        cl = c["client"]
        css = "lab-ok" if cl["outcome"] == "finished" else "lab-bad"
        svg.append(f'<text class="{css}" x="{left}" y="{y0 + 33}">{e(short_outcome(c))}</text>')
        if cl["outcome"] == "finished" and not c["stream"]:
            svg.append(
                f'<text class="ok" x="{x(cl["outcome_at_s"]):.1f}" y="{mid + 5}" text-anchor="middle">✓'
                f"<title>whole answer at {cl['outcome_at_s']:.1f} s</title></text>"
            )
        for t in cl["token_times_s"]:
            svg.append(f'<line class="tok" x1="{x(t):.1f}" y1="{mid - 7}" x2="{x(t):.1f}" y2="{mid + 7}"/>')
        for r in c["phantom_log"]["requests"][1:]:
            svg.append(
                f'<text class="retry" x="{x(r["arrival_s"]):.1f}" y="{mid + 5}" text-anchor="middle">↻'
                f"<title>request #{r['seq']} arrived at {r['arrival_s']:.1f} s</title></text>"
            )
        if cl["outcome"] == "exception":
            svg.append(
                f'<text class="bad" x="{x(cl["outcome_at_s"]):.1f}" y="{mid + 5}" text-anchor="middle">✕'
                f"<title>{e(cl['error_class'])}: {e(cl['error_message'])}</title></text>"
            )
    svg.append("</svg>")

    rows = []
    for c in cells:
        reqs = c["phantom_log"]["requests"]
        detail = "<br>".join(e(phantom_request(r)) for r in reqs)
        rows.append(
            f"<tr><td>{e(c['freeze'])}</td><td>{e(c['config'])}<br><code>{e(c['config_code'])}</code></td>"
            f'<td>{e(user_saw(c))}</td><td class="num">{len(reqs)}</td><td>{detail}</td></tr>'
        )
    scripts = " · ".join(
        f"{e(name)}: <code>{e(' '.join(flags))}</code>" for name, flags in meta["phantom_scripts"].items()
    )
    sub = (
        f"python {e(meta['python'])} · openai {e(meta['openai'])} · httpx2 {e(meta['httpx2'])} · "
        f"phantom {e(meta['phantom'])} · script, plus the freeze: {scripts}"
    )
    return (
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n"
        '<meta name="viewport" content="width=device-width, initial-scale=1">\n'
        f"<title>Provider freeze drill</title>\n<style>{CSS}</style>\n</head>\n<body>\n"
        f"<h1>Provider freeze drill</h1>\n<p class=\"sub\">{sub}</p>\n"
        f'<div class="chart">{"".join(svg)}</div>\n'
        '<p class="legend">Shaded: provider frozen (phantom\'s stall; the clock starts at the first request). '
        "| = token received by the client · ✓ = whole answer received · ✕ = client gave up · "
        "↻ = a further request arrived at phantom.</p>\n"
        '<div class="table"><table>\n<tr><th>freeze</th><th>client</th><th>user saw</th><th>requests</th>'
        f"<th>phantom logged</th></tr>\n{''.join(rows)}\n</table></div>\n</body>\n</html>\n"
    )


def main():
    if not PHANTOM.exists():
        sys.exit(f"build phantom first: cargo build --release -p tailbench-phantom ({PHANTOM})")
    OUT.mkdir(exist_ok=True)
    version = subprocess.run([str(PHANTOM), "--version"], capture_output=True, text=True).stdout.strip()
    meta = {
        "python": platform.python_version(),
        "openai": metadata.version("openai"),
        "httpx2": metadata.version("httpx2"),
        "httpx": metadata.version("httpx") if _has("httpx") else "not installed",
        "phantom": version.removeprefix("phantom ").strip(),
        "phantom_scripts": {"streaming": SCRIPT, "non-streaming": SCRIPT_PLAIN},
        "sdk_defaults": {
            "DEFAULT_TIMEOUT": repr(openai.DEFAULT_TIMEOUT),
            "DEFAULT_MAX_RETRIES": openai.DEFAULT_MAX_RETRIES,
        },
        "cap_s": {"streaming": 40.0, "non-streaming": 120.0},
    }
    print(
        f"python {meta['python']} · openai {meta['openai']} · httpx2 {meta['httpx2']} "
        f"(httpx: {meta['httpx']}) · {version}"
    )
    print(f"SDK defaults: timeout={meta['sdk_defaults']['DEFAULT_TIMEOUT']} max_retries={openai.DEFAULT_MAX_RETRIES}")
    print("phantom script, streaming: " + " ".join(SCRIPT) + " + --stall START:15000 (100 or 600)")
    print("phantom script, non-streaming: " + " ".join(SCRIPT_PLAIN) + " + --stall 0:15000\n", flush=True)
    t0 = time.perf_counter()
    cells = []
    total = len(FREEZES) * len(CONFIGS)
    for freeze, stall, stream, flags, cap_s in FREEZES:
        for cfg_id, cfg_name, cfg_code in CONFIGS:
            cell = run_cell(freeze, stall, stream, flags, cap_s, cfg_id, cfg_name, cfg_code, len(cells))
            cells.append(cell)
            print(
                f"  [{len(cells)}/{total}] {freeze} / {cfg_name}: {user_saw(cell)} "
                f"({cell['client']['wall_s']:.1f} s, {cell['requests_total']} requests)",
                flush=True,
            )
    meta["total_s"] = time.perf_counter() - t0
    (HERE / "drill.json").write_text(json.dumps({"meta": meta, "cells": cells}, indent=2) + "\n")
    (HERE / "drill.html").write_text(render_html(meta, cells))
    print("\nDrill card\n")
    print(card(cells))
    print(f"\ntotal {meta['total_s']:.1f} s · wrote drill.json, drill.html")


def _has(name):
    try:
        metadata.version(name)
        return True
    except metadata.PackageNotFoundError:
        return False


if __name__ == "__main__":
    main()
