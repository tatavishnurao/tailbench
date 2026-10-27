#!/usr/bin/env python3
"""One-off diagnostics for two things the main runs showed. Standard library only.

    python3 diag.py oha    # oha -c 50: what the sockets and kernel counters do during a freeze
    python3 diag.py clock  # vllm bench serve / oha -c 1: wall clock versus monotonic clock

Each step starts from the same server the main runs use (run.py's Server), writes to
out/diag/<step>/ (gitignored) and prints a summary. Nothing here is part of results.json.
Options: --keepalive N sets VLLM_HTTP_TIMEOUT_KEEP_ALIVE for the server (default: unset).
"""

import argparse
import json
import os
import subprocess
import sys
import threading
import time

import run as R

HERE = os.path.dirname(os.path.abspath(__file__))
DIAG = os.path.join(HERE, "out", "diag")

MON = r"""
import sys, time
prev = None
out = open(sys.argv[1], "w")
boot = lambda: time.clock_gettime_ns(time.CLOCK_BOOTTIME)
while True:
    w, m, b = time.time_ns(), time.monotonic_ns(), boot()
    cur = (w - m, b - m)
    if prev is not None and (abs(cur[0] - prev[0]) > 5_000_000 or abs(cur[1] - prev[1]) > 5_000_000):
        out.write(f"{w} wall-minus-mono step {(cur[0]-prev[0])/1e6:.3f} ms; boottime-minus-mono step {(cur[1]-prev[1])/1e6:.3f} ms\n")
        out.flush()
    prev = cur
    time.sleep(0.005)
"""


def netstat_counters():
    """Every counter in /proc/net/snmp and /proc/net/netstat as {"Proto.Name": int}."""
    d = {}
    for path in ("/proc/net/snmp", "/proc/net/netstat"):
        lines = open(path).read().splitlines()
        for names, vals in zip(lines[::2], lines[1::2]):
            proto = names.split(":")[0]
            for k, v in zip(names.split()[1:], vals.split()[1:]):
                try:
                    d[f"{proto}.{k}"] = int(v)
                except ValueError:
                    pass
    return d


def delta(a, b):
    return {k: b[k] - a.get(k, 0) for k in b if b[k] != a.get(k, 0)}


class SsSampler(threading.Thread):
    """`ss -tn` for every TCP socket with port 18000 on either end, every ~100 ms."""

    def __init__(self, path):
        super().__init__(daemon=True)
        self.path, self.stop = path, threading.Event()

    def run(self):
        with open(self.path, "w") as f:
            while not self.stop.is_set():
                t = time.time_ns()
                out = subprocess.run(["ss", "-tnH", "state", "all", f"( sport = :{R.PORT} or dport = :{R.PORT} )"],
                                     capture_output=True, text=True).stdout
                f.write(json.dumps({"t": t, "ss": out.splitlines()}) + "\n")
                time.sleep(0.1)


def summarize_ss(path, t_stop, t_cont):
    """Counts of sockets per (side, state) in windows relative to the freeze."""
    wins = [("before", -1e9, 0), ("freeze first half", 0, 1e9), ("freeze second half", 1e9, 2e9),
            ("after (0-1 s)", 2e9, 3e9), ("after (1-3 s)", 3e9, 5e9)]
    out = {}
    for line in open(path):
        rec = json.loads(line)
        rel = rec["t"] - t_stop
        for name, lo, hi in wins:
            if lo <= rel < hi:
                c = {}
                for row in rec["ss"]:
                    f = row.split()
                    state, rq, sq, local, peer = f[0], f[1], f[2], f[3], f[4]
                    side = "server" if local.endswith(f":{R.PORT}") else "client"
                    c[(side, state)] = c.get((side, state), 0) + 1
                    if side == "server" and state in ("ESTAB", "SYN-RECV") and int(rq) > 0:
                        c[(side, state + " Recv-Q>0")] = c.get((side, state + " Recv-Q>0"), 0) + 1
                out.setdefault(name, []).append(c)
    return out


def peak(samples):
    keys = {k for s in samples for k in s}
    return {f"{a} {b}": max(s.get((a, b), 0) for s in samples) for a, b in sorted(keys)}


def run_step(server, cfg_id, tag, freeze):
    """Run one configuration with counters, ss sampling and (optionally) the freeze."""
    d = os.path.join(DIAG, tag)
    os.makedirs(d, exist_ok=True)
    R.OUT = DIAG  # one_run writes to OUT/runs/<id>; keep the diag runs separate from the main ones
    before = netstat_counters()
    off = server.log_size()
    sampler = SsSampler(os.path.join(d, "ss.jsonl"))
    sampler.start()
    if freeze:
        rec = R.one_run(server, cfg_id, None, tag, server.baseline_ms)
        d = os.path.join(DIAG, "runs", f"{cfg_id}-rep{tag}")  # the tool's output lives here
        os.replace(os.path.join(DIAG, tag, "ss.jsonl"), os.path.join(d, "ss.jsonl"))
    else:
        rec = run_no_freeze(server, cfg_id, d)
    sampler.stop.set()
    sampler.join()
    after = netstat_counters()
    with open(os.path.join(R.OUT, "server.log"), "rb") as f:
        f.seek(off)
        slog = f.read().decode(errors="replace")
    json.dump(dict(rec=rec, counters=delta(before, after)), open(os.path.join(d, "result.json"), "w"), indent=1, default=str)
    open(os.path.join(d, "server-log-slice.txt"), "w").write(slog)
    return rec, delta(before, after), slog, d


def run_no_freeze(server, cfg_id, d):
    cfg = R.configs(d)[cfg_id]
    t0 = time.time_ns()
    with open(f"{d}/stdout.txt", "wb") as so, open(f"{d}/stderr.txt", "wb") as se:
        p = subprocess.run(cfg["cmd"], stdout=so, stderr=se, env=R.venv_env(), stdin=subprocess.DEVNULL, timeout=200)
    rec = dict(id=cfg_id, tool=cfg["label"], command=cfg["cmd"], wall_s=(time.time_ns() - t0) / 1e9, exit_code=p.returncode)
    rec["reported"] = R.parse_tool(cfg["parse"], d, f"{d}/stdout.txt")
    return rec


def start_server(keepalive):
    os.makedirs(DIAG, exist_ok=True)
    R.OUT = DIAG
    if keepalive is not None:
        R.SERVER_ENV["VLLM_HTTP_TIMEOUT_KEEP_ALIVE"] = str(keepalive)
    s = R.Server()
    s.start()
    return s


def oha_step(server, label, freeze):
    rec, cnt, slog, d = run_step(server, "C2", label, freeze)
    rep = rec.get("reported") or {}
    print(f"\n=== {label}: oha -c 50, freeze={freeze}")
    if freeze:
        print(f"freeze {rec['freeze_s']:.3f} s, STOP {(rec['freeze_stop_ns'] - rec['t_first_post_ns']) / 1e9:.3f} s after first POST seen")
    print("oha reported:", {k: (round(v, 1) if isinstance(v, float) else v) for k, v in rep.items()})
    oj = json.load(open(f"{d}/stdout.txt"))
    print("oha statusCodeDistribution:", oj["statusCodeDistribution"], " errorDistribution:", oj["errorDistribution"])
    print("kernel counter deltas (nonzero, TCP-related):")
    for k, v in sorted(cnt.items()):
        if k.startswith(("Tcp.", "TcpExt.")) and not k.endswith(("InSegs", "OutSegs")):
            print(f"  {k} {v}")
    bad = [ln for ln in slog.splitlines() if "POST /v1/completions" not in ln and ("ERROR" in ln or "WARN" in ln or "Error" in ln or "Traceback" in ln)]
    posts = slog.count("POST /v1/completions")
    print(f"server log slice: {posts} POST lines, {len(bad)} ERROR/WARN lines")
    for ln in bad[:10]:
        print("   ", ln[:200])
    if freeze:
        ss = summarize_ss(f"{d}/ss.jsonl", rec["freeze_stop_ns"], rec["freeze_cont_ns"])
        print("peak sockets per (side, state) in each window:")
        for name, samples in ss.items():
            print(f"  {name}: {peak(samples)}")
    else:
        every = []
        for ln in open(f"{d}/ss.jsonl"):
            c = {}
            for row in json.loads(ln)["ss"]:
                f = row.split()
                k = ("server" if f[3].endswith(f":{R.PORT}") else "client", f[0])
                c[k] = c.get(k, 0) + 1
            every.append(c)
        print("peak sockets per (side, state) over the whole run:", peak(every))
    return rec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("step", choices=["oha", "clock", "soak"])
    ap.add_argument("--keepalive", type=int)
    args = ap.parse_args()
    server = start_server(args.keepalive)
    mon = None
    try:
        if args.step == "oha":
            for label, freeze in (("c50-freeze-a", True), ("c50-freeze-b", True), ("c50-nofreeze", False)):
                time.sleep(10)
                oha_step(server, label + ("-ka%d" % args.keepalive if args.keepalive else ""), freeze)
        else:
            mon = subprocess.Popen([sys.executable, "-I", "-c", MON, os.path.join(DIAG, "clock-steps.txt")], start_new_session=True)
            plan = (("E", True), ("E", False), ("C", True)) if args.step == "clock" else \
                (("E", True), ("A", True), ("E", True), ("B", True), ("E", True), ("A", True), ("E", True))
            for n, (cid, freeze) in enumerate(plan):
                time.sleep(10)
                tag = f"{args.step}-{n}-{cid}-{'freeze' if freeze else 'nofreeze'}"
                rec, cnt, slog, d = run_step(server, cid, tag, freeze)
                rep = rec["reported"]
                print(f"\n=== {tag}: {rec['tool']}: reported {rep}")
                if freeze:
                    print(f"t_stop - t_first_post (wall) = {rec['freeze_after_first_s']:.3f} s (the orchestrator sleeps on the monotonic clock for 8.0 s)")
                print("wall-clock steps so far:", open(os.path.join(DIAG, "clock-steps.txt")).read().strip() or "none")
    finally:
        if mon:
            mon.terminate()
        server.stop()


if __name__ == "__main__":
    main()
