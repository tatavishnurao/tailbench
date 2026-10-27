#!/usr/bin/env python3
"""Combine the raw output of both passes with expected.py into results.json.

Standard library only. Inputs: out/pass1/ and out/ (pass 2), each with runs-raw.json
(from run.py) and runs/<id>/ (the tools' own output). Outputs: results.json, and the
tables between <!-- BEGIN:name --> / <!-- END:name --> markers in README.md.

Pass 2 is the headline (see README, "Two passes"); pass 1 is reported next to it.
"""

import json
import math
import os
import re
import statistics as st
import sys

import expected as ex

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "out")
KEYS = ("p50", "p99", "max")
ORDER = ("A", "B", "C", "D", "C2", "D2", "E", "F", "G")
OHA = ("C", "D", "C2", "D2")
SCHEDULED = 250  # 10 req/s for 25 s


def load_pass(name, raw_path, runs_dir):
    raw = json.load(open(raw_path))
    return dict(name=name, meta=raw["meta"], raw=raw["runs"], runs_dir=runs_dir)


def oha_detail(run_dir):
    """What oha itself wrote: the status and error distributions, success rate, p99.9."""
    d = json.load(open(os.path.join(run_dir, "stdout.txt")))
    err = d["errorDistribution"]
    return dict(
        ok=sum(d["statusCodeDistribution"].values()),
        status_codes=d["statusCodeDistribution"],
        error_distribution=err,
        connection_errors=err.get("connection error", 0),
        deadline_aborts=err.get("aborted due to deadline", 0),
        other_errors=sum(v for k, v in err.items() if k not in ("connection error", "aborted due to deadline")),
        success_rate=d["summary"]["successRate"],
        p99_9_ms=d["latencyPercentiles"]["p99.9"] * 1e3,
        mean_latency_ms=d["summary"]["average"] * 1e3,
        total_s=d["summary"]["total"],
    )


def freeze_ticks(run):
    """Schedule slots (i / 10 s from the tool's start) that fall inside [STOP, CONT]."""
    t0 = run["t_tool_start_ns"]
    f0, f1 = (run["freeze_stop_ns"] - t0) / 1e9, (run["freeze_cont_ns"] - t0) / 1e9
    return f0, f1, sum(1 for i in range(SCHEDULED) if f0 <= i * ex.PERIOD <= f1)


def fifo_completions(f0, f1, s0, r, deadline=25.0):
    """Requests a single FIFO connection finishes by the deadline (oha -c 1 -q 10 -z 25s)."""
    free, done = 0.0, 0
    for i in range(SCHEDULED):
        t = i * ex.PERIOD
        start = max(t, free)
        end = start + s0
        if f0 <= start < f1:
            start, end = f1, f1 + s0 + r
        elif start < f0 < end:
            end += (f1 - f0) + r
        free = end
        if end > deadline:
            break
        done += 1
    return done


def build_pass(p, cal):
    r = cal["r_ms"] / 1e3
    start_s0 = p["meta"]["baseline_ms"] / 1e3
    out = []
    for run in p["raw"]:
        rec = {k: run.get(k) for k in (
            "id", "config", "rep", "tool", "version", "command", "concurrency", "metric", "freeze_stop_ns",
            "freeze_cont_ns", "freeze_s", "freeze_after_first_s", "t_first_post_ns", "server_posts",
            "exit_code", "wall_s", "server_restarted_before", "error")}
        rec["pass"] = p["name"]
        rep = run.get("reported")
        rec["reported"] = rep
        model, lat = run["expected_model"], run["expected_latency"]
        rec["expected_model"], rec["expected_latency"] = model, lat
        if not rep:
            out.append(rec)
            continue
        if run.get("s0_pre_ms") is not None:
            s0 = (run["s0_pre_ms"] + run["s0_post_ms"]) / 2e3
            rec.update(s0_pre_ms=run["s0_pre_ms"], s0_post_ms=run["s0_post_ms"], s0_used_ms=s0 * 1e3,
                       s0_source="mean of the median-of-5 plain requests just before and just after this run")
        else:
            s0 = start_s0
            rec.update(s0_used_ms=s0 * 1e3, s0_source="median of 10 plain requests when this pass's server started "
                       "(pass 1 did not measure s0 per run)")
        fl = run["freeze_s"]
        pa = ex.expected_phase_averaged(model, fl, s0, r, freeze_after=run["freeze_after_first_s"])[lat]
        rec["expected_phase_averaged"] = pa
        rec["reported_minus_phase_averaged"] = {k: rep[k] - pa[k] for k in KEYS}
        if run["config"] == "A":
            t0 = run["t_first_post_ns"]
            origin = (rep["origin_unix_ns"] - t0) / 1e9
            f0, f1 = (run["freeze_stop_ns"] - t0) / 1e9, (run["freeze_cont_ns"] - t0) / 1e9
            exact = ex.expected(model, origin, f0, f1, s0, r)[lat]
            rec["expected_exact"] = exact
            rec["reported_minus_exact"] = {k: rep[k] - exact[k] for k in KEYS}
        if run["config"] in OHA:
            det = oha_detail(os.path.join(p["runs_dir"], run["id"]))
            rec["oha"] = det
            f0, f1, ticks = freeze_ticks(run)
            rec["count_check"] = dict(
                scheduled=SCHEDULED, ok=det["ok"], shortfall=SCHEDULED - det["ok"],
                freeze_start_after_tool_start_s=f0, freeze_end_after_tool_start_s=f1, arrivals_in_freeze=ticks,
                sum_latency_s=det["ok"] * det["mean_latency_ms"] / 1e3)
            if run["config"] in ("C", "D"):
                rec["count_check"]["fifo_sim_ok"] = fifo_completions(f0, f1, s0, r)
        out.append(rec)
    return out


def s0_summary(passes, cal):
    rows = [("gate (STEP 0)", "5 requests, mean", cal["s0_ms"], cal["s0_ms"], cal["s0_ms"])]
    for p in passes:
        b = p["meta"]["baseline_ms"]
        rows.append((f"{p['name']}: at server start", "median of 10", b, b, b))
        pre = [x["s0_pre_ms"] for x in p["raw"] if x.get("s0_pre_ms") is not None]
        post = [x["s0_post_ms"] for x in p["raw"] if x.get("s0_post_ms") is not None]
        if pre:
            rows.append((f"{p['name']}: per run, before", f"median of 5, n={len(pre)}", st.median(pre), min(pre), max(pre)))
            rows.append((f"{p['name']}: per run, after", f"median of 5, n={len(post)}", st.median(post), min(post), max(post)))
        else:
            rows.append((f"{p['name']}: per run", "not measured", None, None, None))
    return rows


def f1(x, d=1):
    return "-" if x is None else f"{x:.{d}f}"


def main_table(runs):
    L = ["| cfg | tool | rep | n ok | errors | p50 ms | p99 ms | max ms | expected p99 (phase-avg) | expected max | s0 used ms |",
         "|:--|:--|--:|--:|--:|--:|--:|--:|--:|--:|--:|"]
    for x in runs:
        rp, pa = x["reported"], x.get("expected_phase_averaged")
        if not rp:
            L.append(f"| {x['config']} | {x['tool']} | {x['rep']} | - | - | - | - | - | - | - | - |")
            continue
        errs = rp["errors"]
        if "oha" in x:
            o = x["oha"]
            errs = f"{o['connection_errors']} conn + {o['deadline_aborts']} deadline" if o["connection_errors"] or o["other_errors"] else f"{o['deadline_aborts']} deadline"
        L.append(f"| {x['config']} | {x['tool']} | {x['rep']} | {rp['n']} | {errs} | {rp['p50']:.1f} | {rp['p99']:.1f} | "
                 f"{rp['max']:.1f} | {pa['p99']:.1f} | {pa['max']:.1f} | {x['s0_used_ms']:.1f} |")
    return "\n".join(L)


def headline_table(runs2, runs1):
    """One row per configuration: p99 and max of each repetition, the mean expected p99, pass 1 p99s."""
    by2, by1 = {}, {}
    for src, dst in ((runs2, by2), (runs1, by1)):
        for x in src:
            if x["reported"]:
                dst.setdefault(x["config"], []).append(x)
    L = ["| cfg | tool | reported p99 ms, pass 2 (rep 1 / rep 2) | reported max ms, pass 2 | expected p99 ms (phase-avg) | reported p99 ms, pass 1 | oha connection errors, pass 2 |",
         "|:--|:--|:--|:--|--:|:--|:--|"]
    for c, rs in by2.items():
        exp = sum(x["expected_phase_averaged"]["p99"] for x in rs) / len(rs)
        errs = " / ".join(str(x["oha"]["connection_errors"]) for x in rs) if "oha" in rs[0] else "-"
        L.append(f"| {c} | {rs[0]['tool']} | {' / '.join(f'{x['reported']['p99']:.0f}' for x in rs)} | "
                 f"{' / '.join(f'{x['reported']['max']:.0f}' for x in rs)} | {exp:.0f} | "
                 f"{' / '.join(f'{x['reported']['p99']:.0f}' for x in by1.get(c, []))} | {errs} |")
    return "\n".join(L)


def count_table(runs_by_pass):
    L = ["| pass | run | n ok | shortfall vs 250 | freeze arrivals | shortfall = arrivals? | Σ latency s | FIFO sim n ok |",
         "|:--|:--|--:|--:|--:|:--|--:|--:|"]
    for name, runs in runs_by_pass:
        for x in runs:
            if x["config"] not in ("C", "D") or "count_check" not in x:
                continue
            c = x["count_check"]
            eq = "yes" if c["shortfall"] == c["arrivals_in_freeze"] else f"no ({c['shortfall'] - c['arrivals_in_freeze']:+d})"
            L.append(f"| {name} | {x['config']} rep {x['rep']} | {c['ok']} | {c['shortfall']} | {c['arrivals_in_freeze']} | {eq} | "
                     f"{c['sum_latency_s']:.1f} | {c['fifo_sim_ok']} |")
    return "\n".join(L)


def c50_table(runs_by_pass):
    L = ["| pass | run | oha errorDistribution (verbatim) | ok | errors / intended | oha p99 ms | expected p99 ms | oha p99.9 ms | oha max ms |",
         "|:--|:--|:--|--:|--:|--:|--:|--:|--:|"]
    for name, runs in runs_by_pass:
        for x in runs:
            if x["config"] not in ("C2", "D2") or "oha" not in x:
                continue
            o, c, rp = x["oha"], x["count_check"], x["reported"]
            errs = o["connection_errors"] + o["other_errors"]
            L.append(f"| {name} | {x['config']} rep {x['rep']} | `{json.dumps(o['error_distribution'])}` | {o['ok']} | "
                     f"{errs} / {c['arrivals_in_freeze']} | {rp['p99']:.1f} | {x['expected_phase_averaged']['p99']:.1f} | "
                     f"{o['p99_9_ms']:.1f} | {rp['max']:.1f} |")
    return "\n".join(L)


def s0_table(rows):
    L = ["| s0 measurement | method | median ms | min ms | max ms |", "|:--|:--|--:|--:|--:|"]
    for name, method, med, lo, hi in rows:
        L.append(f"| {name} | {method} | {f1(med)} | {f1(lo)} | {f1(hi)} |")
    return "\n".join(L)


def ka_rows():
    path = os.path.join(OUT, "diag-ka", "summary.json")
    return json.load(open(path)) if os.path.exists(path) else []


def ka_table(rows):
    L = ["| run | server keep-alive | errorDistribution (verbatim) | conn. errors | server ESTAB before STOP / 0.5 s after CONT | EstabResets / OutRsts / TCPAbortOnClose | oha p99 ms | s0 before / after ms | GPU sm MHz mean (min-max) | GPU temp max C | GPU power mean W |",
         "|:--|:--|:--|--:|:--|:--|--:|:--|:--|--:|--:|"]
    for x in rows:
        c, ss, g = x["counters"], x["ss"], x["gpu"] or {}
        L.append(f"| {x['tag']} ({x['tool']}) | {'300 s' if x['tag'].endswith('ka300') else '5 s (default)'} | "
                 f"{('`' + json.dumps(x['errorDistribution']) + '`') if 'errorDistribution' in x else '-'} | {x.get('connection_errors', '-')} | "
                 f"{ss['server_estab_before_stop']} / {ss['server_estab_0p5s_after_cont']} | "
                 f"{c.get('Tcp.EstabResets', 0)} / {c.get('Tcp.OutRsts', 0)} / {c.get('TcpExt.TCPAbortOnClose', 0)} | "
                 f"{f1(x.get('oha_p99_ms', x['reported'].get('p99')))} | {x['s0_pre_ms']:.0f} / {x['s0_post_ms']:.0f} | "
                 f"{g.get('sm_mhz_mean', 0):.0f} ({g.get('sm_mhz_min', 0):.0f}-{g.get('sm_mhz_max', 0):.0f}) | {g.get('temp_c_max', 0):.0f} | {g.get('power_w_mean', 0):.1f} |")
    return "\n".join(L)


def inject(readme_path, tables):
    if not os.path.exists(readme_path):
        return False
    text = open(readme_path).read()
    for name, body in tables.items():
        pat = re.compile(rf"(<!-- BEGIN:{name} -->\n).*?(<!-- END:{name} -->)", re.S)
        if not pat.search(text):
            print(f"README has no markers for {name}", file=sys.stderr)
            continue
        text = pat.sub(lambda m: m.group(1) + body + "\n" + m.group(2), text)
    open(readme_path, "w").write(text)
    return True


def main():
    cal = ex.load_calibration()
    p1 = load_pass("pass 1", os.path.join(OUT, "pass1", "runs-raw.json"), os.path.join(OUT, "pass1", "runs"))
    p2 = load_pass("pass 2", os.path.join(OUT, "runs-raw.json"), os.path.join(OUT, "runs"))
    runs1, runs2 = build_pass(p1, cal), build_pass(p2, cal)
    s0rows = s0_summary([p1, p2], cal)
    meta = dict(p2["meta"], headline_pass="pass 2", s0_gate_ms=cal["s0_ms"], r_ms=cal["r_ms"], r_source=cal["r_source"],
                s0_gate_source=cal["s0_source"], pass1_meta=p1["meta"], keepalive_test=ka_rows(),
                s0_rows=[dict(name=a, method=b, median_ms=c, min_ms=d, max_ms=e) for a, b, c, d, e in s0rows])
    # "runs" is the headline pass (chart.py reads it); "pass1_runs" is the other one.
    json.dump(dict(meta=meta, runs=runs2, pass1_runs=runs1), open(os.path.join(HERE, "results.json"), "w"), indent=1)
    tables = dict(headline=headline_table(runs2, runs1), main_pass2=main_table(runs2), main_pass1=main_table(runs1), s0=s0_table(s0rows),
                  counts=count_table([("pass 1", runs1), ("pass 2", runs2)]),
                  c50=c50_table([("pass 1", runs1), ("pass 2", runs2)]), ka=ka_table(ka_rows()))
    for k, v in tables.items():
        print(f"\n## {k}\n{v}")
    ok = inject(os.path.join(HERE, "README.md"), tables)
    print("\nresults.json written;", len(runs2), "+", len(runs1), "runs;", "README tables updated" if ok else "no README yet")


if __name__ == "__main__":
    sys.exit(main())
