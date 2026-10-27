#!/usr/bin/env python3
"""results.json -> chart.html. Standard library only; no JavaScript.

One row per configuration, pass 2 (the headline pass), each repetition drawn
on its own line with nothing averaged: a thin bar for the reported p99 (r1
above, r2 below), a small diamond for the reported max, a tick for that
repetition's expected p99 (phase-averaged), and a line at the freeze length,
all on one linear millisecond axis. The values, with oha's connection errors,
are written in a column right of the plot, so no value label sits on a marker.
The table under the chart lists every repetition of pass 2 and pass 1.
"""

import html
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
SUB_H = 11  # one repetition's bar
GAP = 3  # between r1 and r2
ROW_H = 2 * SUB_H + GAP + 18
LEFT = 250
VALS = 250  # value column, right of the plot
W = 1000
TOP = 36


def main():
    res = json.load(open(os.path.join(HERE, "results.json")))
    meta, runs, runs1 = res["meta"], res["runs"], res["pass1_runs"]
    by = {}
    for r in runs:
        if r["reported"]:
            by.setdefault(r["config"], []).append(r)
    order = [c for c in ("A", "B", "C", "D", "C2", "D2", "E", "F", "G") if c in by]
    rows = []
    for c in order:
        rs = sorted(by[c], key=lambda r: r["rep"])
        rows.append(dict(c=c, tool=rs[0]["tool"], reps=[dict(
            rep=r["rep"], p99=r["reported"]["p99"], mx=r["reported"]["max"], exp=r["expected_phase_averaged"]["p99"],
            err=r["oha"]["connection_errors"] if "oha" in r else None) for r in rs]))
    axis_max = max(2400, max(p["mx"] for r in rows for p in r["reps"]) * 1.03)
    plot_r = W - VALS
    x = lambda v: LEFT + v / axis_max * (plot_r - LEFT)
    h = TOP + ROW_H * len(rows) + 30
    s = [f'<svg viewBox="0 0 {W} {h}" width="{W}" height="{h}" role="img" aria-label="Reported p99 and max of each repetition per tool, with the freeze length and expected p99">']
    t = 0
    while t <= axis_max:
        s.append(f'<line class="grid" x1="{x(t):.1f}" x2="{x(t):.1f}" y1="{TOP - 6}" y2="{h - 26}"/>')
        s.append(f'<text class="tick" x="{x(t):.1f}" y="{h - 10}" text-anchor="middle">{t}</text>')
        t += 500
    s.append(f'<text class="tick" x="{LEFT - 10}" y="{h - 10}" text-anchor="end">milliseconds</text>')
    s.append(f'<text class="tick" x="{plot_r + 16}" y="{TOP - 12}">reported, ms</text>')
    for i, r in enumerate(rows):
        y = TOP + i * ROW_H
        s.append(f'<text class="label" x="{LEFT - 10}" y="{y + SUB_H + 2}" text-anchor="end">{html.escape(r["c"])} · {html.escape(r["tool"])}</text>')
        for k, p in enumerate(r["reps"]):
            yb = y + k * (SUB_H + GAP)
            tip = f'{html.escape(r["tool"])} rep {p["rep"]}: reported p99 {p["p99"]:.0f} ms, max {p["mx"]:.0f} ms, expected p99 {p["exp"]:.0f} ms'
            s.append(f'<rect class="bar r{k + 1}" x="{LEFT}" y="{yb}" width="{max(1, x(p["p99"]) - LEFT):.1f}" height="{SUB_H}" rx="1.5"><title>{tip}</title></rect>')
            ex = x(p["exp"])
            s.append(f'<line class="exp" x1="{ex:.1f}" x2="{ex:.1f}" y1="{yb - 1.5}" y2="{yb + SUB_H + 1.5}"><title>{tip}</title></line>')
            cx, cy = x(p["mx"]), yb + SUB_H / 2
            s.append(f'<path class="max" d="M{cx:.1f} {cy - 5.5:.1f} l4.5 5.5 l-4.5 5.5 l-4.5 -5.5 z"><title>{tip}</title></path>')
            err = f' · {p["err"]} conn. err.' if p["err"] is not None else ""
            s.append(f'<text class="val" x="{plot_r + 16}" y="{yb + SUB_H - 1.5}"><tspan class="rl">r{p["rep"]}</tspan> p99 {p["p99"]:.0f} · max {p["mx"]:.0f}{err}</text>')
    s.append(f'<line class="freeze" x1="{x(2000):.1f}" x2="{x(2000):.1f}" y1="{TOP - 6}" y2="{h - 26}"/>')
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
           f'{meta["rate"]} req/s for {meta["duration_s"]} s · {meta["reps"]} repetitions per pass · chart: pass 2, each repetition')
    page = f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Real server freeze</title>
<style>
:root {{ --bg:#ffffff; --fg:#1b1b1b; --muted:#666666; --grid:#e4e4e4; --bar:#1f5fbf; --bar2:#7aa5e6; --max:#1b1b1b; --exp:#1a7f37; --freeze:#d1242f; --row:#f5f5f5; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#16181b; --fg:#e8e8e8; --muted:#9a9a9a; --grid:#2e3236; --bar:#6ea8ff; --bar2:#3b6db5; --max:#e8e8e8; --exp:#4ac26b; --freeze:#ff6b6b; --row:#1f2226; }} }}
body {{ background:var(--bg); color:var(--fg); font:15px/1.5 system-ui,sans-serif; margin:0 auto; max-width:1040px; padding:16px; }}
h1 {{ font-size:1.5rem; margin:.2em 0; }}
p.sub {{ color:var(--muted); margin:0 0 1em; }}
.wrap {{ overflow-x:auto; }}
svg {{ display:block; }}
.grid {{ stroke:var(--grid); stroke-width:1; }}
.tick {{ fill:var(--muted); font-size:11px; }}
.label {{ fill:var(--fg); font-size:12px; }}
.rl {{ fill:var(--muted); font-weight:600; }}
.bar {{ fill:var(--bar); }}
.bar.r2 {{ fill:var(--bar2); }}
.val {{ fill:var(--fg); font-size:11px; }}
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
<p class="legend">Pass 2, nothing averaged: each configuration has one line per repetition, <b>r1</b> upper (darker bar) and <b>r2</b> lower (lighter bar). <b>bar</b> reported p99 · <b>◆</b> reported max · <b style="color:var(--exp)">|</b> that repetition's expected p99, phase-averaged (no tool involved) · <b style="color:var(--freeze)">- -</b> the 2000 ms freeze. The column on the right gives each repetition's p99 and max and, for oha, its connection errors. Pass 1 is in the table only. Table: pass 2 first, then pass 1.</p>
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
