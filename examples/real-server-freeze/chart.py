#!/usr/bin/env python3
"""results.json -> chart.html. Standard library only; no JavaScript.

One row per configuration: a bar for the reported p99, a marker for the
reported max, a tick for the expected p99 (phase-averaged), a ring for the
same tool's pass-1 p99, a line at the freeze length, all on one linear
millisecond axis. Bars, markers and ticks are pass 2 (the headline pass), the
mean of its repetitions; the table under the chart lists every repetition of
pass 2 and pass 1.
"""

import html
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
ROW_H = 38
LEFT = 250
RIGHT = 20
W = 900
TOP = 36


def mean(xs):
    return sum(xs) / len(xs)


def main():
    res = json.load(open(os.path.join(HERE, "results.json")))
    meta, runs, runs1 = res["meta"], res["runs"], res["pass1_runs"]
    by, by1 = {}, {}
    for r in runs:
        if r["reported"]:
            by.setdefault(r["config"], []).append(r)
    for r in runs1:
        if r["reported"]:
            by1.setdefault(r["config"], []).append(r)
    order = [c for c in ("A", "B", "C", "D", "C2", "D2", "E", "F", "G") if c in by]
    rows = []
    for c in order:
        rs = by[c]
        rows.append(dict(
            c=c, tool=rs[0]["tool"], p99=mean([x["reported"]["p99"] for x in rs]),
            mx=mean([x["reported"]["max"] for x in rs]),
            exp=mean([x["expected_phase_averaged"]["p99"] for x in rs]), n=len(rs),
            p1=mean([x["reported"]["p99"] for x in by1[c]]) if c in by1 else None,
            err=[x["oha"]["connection_errors"] for x in rs] if "oha" in rs[0] and any(x["oha"]["connection_errors"] for x in rs + by1.get(c, [])) else None))
    axis_max = max(2400, max(r["mx"] for r in rows) * 1.05)
    plot_w = W - LEFT - RIGHT
    x = lambda v: LEFT + v / axis_max * plot_w
    h = TOP + ROW_H * len(rows) + 38
    s = [f'<svg viewBox="0 0 {W} {h}" width="{W}" height="{h}" role="img" aria-label="Reported p99 and max per tool, with the freeze length and expected p99">']
    step = 500
    t = 0
    while t <= axis_max:
        s.append(f'<line class="grid" x1="{x(t):.1f}" x2="{x(t):.1f}" y1="{TOP - 6}" y2="{h - 28}"/>')
        s.append(f'<text class="tick" x="{x(t):.1f}" y="{h - 12}" text-anchor="middle">{t}</text>')
        t += step
    s.append(f'<text class="tick" x="{LEFT - 10}" y="{h - 12}" text-anchor="end">milliseconds</text>')
    for i, r in enumerate(rows):
        y = TOP + i * ROW_H
        s.append(f'<text class="label" x="{LEFT - 10}" y="{y + 12}" text-anchor="end">{html.escape(r["c"])} · {html.escape(r["tool"])}</text>')
        if r["err"] is not None:
            s.append(f'<text class="note" x="{LEFT - 10}" y="{y + 24}" text-anchor="end">connection errors per rep: {" / ".join(str(e) for e in r["err"])}</text>')
        if r["p1"] is not None:
            s.append(f'<circle class="p1" cx="{x(r["p1"]):.1f}" cy="{y + 11}" r="5"><title>pass 1 reported p99 {r["p1"]:.0f} ms</title></circle>')
        s.append(f'<rect class="bar" x="{LEFT}" y="{y + 3}" width="{max(1, x(r["p99"]) - LEFT):.1f}" height="16" rx="2"><title>{html.escape(r["tool"])}: reported p99 {r["p99"]:.0f} ms</title></rect>')
        s.append(f'<text class="val" x="{x(r["p99"]) + 4:.1f}" y="{y + 15}" >{r["p99"]:.0f}</text>' if r["p99"] < axis_max * 0.85 else
                 f'<text class="valin" x="{x(r["p99"]) - 4:.1f}" y="{y + 15}" text-anchor="end">{r["p99"]:.0f}</text>')
        cx = x(r["mx"])
        s.append(f'<path class="max" d="M{cx:.1f} {y + 2} l6 10 l-6 10 l-6 -10 z"><title>reported max {r["mx"]:.0f} ms</title></path>')
        s.append(f'<line class="exp" x1="{x(r["exp"]):.1f}" x2="{x(r["exp"]):.1f}" y1="{y}" y2="{y + 24}"><title>expected p99 {r["exp"]:.0f} ms</title></line>')
    s.append(f'<line class="freeze" x1="{x(2000):.1f}" x2="{x(2000):.1f}" y1="{TOP - 6}" y2="{h - 28}"/>')
    s.append(f'<text class="freezet" x="{x(2000) + 4:.1f}" y="{TOP - 12}">freeze 2000 ms</text>')
    s.append("</svg>")
    svg = "\n".join(s)

    trs = []
    for r in runs + runs1:
        rp = r["reported"]
        if not rp:
            trs.append(f'<tr><td>{r["pass"][-1]}</td><td>{r["config"]}</td><td>{html.escape(r["tool"])}</td><td>{r["rep"]}</td><td colspan="6">no result: {html.escape(str(r.get("error")))}</td><td>{html.escape(r["metric"])}</td></tr>')
            continue
        pa = r["expected_phase_averaged"]
        if "oha" in r:
            o = r["oha"]
            errs = f'{o["connection_errors"]} conn, {o["deadline_aborts"]} deadline'
        else:
            errs = str(rp["errors"])
        trs.append(
            f'<tr><td>{r["pass"][-1]}</td><td>{r["config"]}</td><td>{html.escape(r["tool"])}</td><td>{r["rep"]}</td><td>{rp["n"]}</td>'
            f'<td>{errs}</td><td>{rp["p50"]:.1f}</td><td>{rp["p99"]:.1f}</td><td>{rp["max"]:.1f}</td>'
            f'<td>{pa["p99"]:.1f}</td><td>{pa["max"]:.1f}</td><td>{html.escape(r["metric"])}</td></tr>')
    sub = (f'{html.escape(meta["gpu"])} · {html.escape(meta["model"])} · vLLM {html.escape(meta["vllm"])} · '
           f'freeze {meta["freeze_s"]:g} s (SIGSTOP/SIGCONT, {meta["freeze_after_first_s"]:g} s after the first request) · '
           f'{meta["rate"]} req/s for {meta["duration_s"]} s · {meta["reps"]} repetitions per pass · bars are pass 2')
    page = f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Real server freeze</title>
<style>
:root {{ --bg:#ffffff; --fg:#1b1b1b; --muted:#666666; --grid:#e4e4e4; --bar:#1f5fbf; --max:#1b1b1b; --exp:#1a7f37; --freeze:#d1242f; --row:#f5f5f5; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#16181b; --fg:#e8e8e8; --muted:#9a9a9a; --grid:#2e3236; --bar:#6ea8ff; --max:#e8e8e8; --exp:#4ac26b; --freeze:#ff6b6b; --row:#1f2226; }} }}
body {{ background:var(--bg); color:var(--fg); font:15px/1.5 system-ui,sans-serif; margin:0 auto; max-width:960px; padding:16px; }}
h1 {{ font-size:1.5rem; margin:.2em 0; }}
p.sub {{ color:var(--muted); margin:0 0 1em; }}
.wrap {{ overflow-x:auto; }}
svg {{ display:block; min-width:640px; }}
.grid {{ stroke:var(--grid); stroke-width:1; }}
.tick {{ fill:var(--muted); font-size:11px; }}
.label {{ fill:var(--fg); font-size:12px; }}
.note {{ fill:var(--muted); font-size:10px; }}
.p1 {{ fill:none; stroke:var(--fg); stroke-width:1.5; }}
.bar {{ fill:var(--bar); }}
.val {{ fill:var(--fg); font-size:11px; }}
.valin {{ fill:#fff; font-size:11px; }}
.max {{ fill:var(--max); stroke:var(--bg); stroke-width:1; }}
.exp {{ stroke:var(--exp); stroke-width:3; }}
.freeze {{ stroke:var(--freeze); stroke-width:2; stroke-dasharray:5 3; }}
.freezet {{ fill:var(--freeze); font-size:11px; }}
.legend {{ color:var(--muted); font-size:13px; margin:.5em 0 1.2em; }}
.legend b {{ color:var(--fg); }}
table {{ border-collapse:collapse; font-size:13px; width:100%; }}
th,td {{ text-align:right; padding:4px 8px; border-bottom:1px solid var(--grid); }}
th:nth-child(3),td:nth-child(3),th:nth-child(6),td:nth-child(6),th:last-child,td:last-child {{ text-align:left; }}
tbody tr:nth-child(even) {{ background:var(--row); }}
</style></head><body>
<h1>Real server freeze</h1>
<p class="sub">{sub}</p>
<div class="wrap">{svg}</div>
<p class="legend"><b>bar</b> reported p99 (mean of the repetitions) · <b>◆</b> reported max · <b style="color:var(--exp)">|</b> expected p99, phase-averaged (no tool involved) · <b>○</b> the same tool\'s pass-1 p99 · <b style="color:var(--freeze)">- -</b> the 2000 ms freeze. Table: pass 2 first, then pass 1.</p>
<div class="wrap"><table>
<thead><tr><th>pass</th><th>cfg</th><th>tool</th><th>rep</th><th>n ok</th><th>errors</th><th>p50 ms</th><th>p99 ms</th><th>max ms</th><th>exp. p99</th><th>exp. max</th><th>metric used</th></tr></thead>
<tbody>
{chr(10).join(trs)}
</tbody></table></div>
</body></html>
"""
    open(os.path.join(HERE, "chart.html"), "w").write(page)
    print("chart.html written")


if __name__ == "__main__":
    main()
