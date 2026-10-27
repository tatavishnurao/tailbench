#!/usr/bin/env python3
"""Keep-alive and connection-count test, plus a GPU clock log. One-off; standard library only.

    python3 diag_ka.py

oha -q 10 -z 25s --latency-correction with the same 2.0 s freeze, one run each:
-c 20, -c 100, -c 50 and a second -c 50 at the default VLLM_HTTP_TIMEOUT_KEEP_ALIVE (one
server, health check and 5 s gap between runs), then a second server with
VLLM_HTTP_TIMEOUT_KEEP_ALIVE=300 and one -c 50 run. The environment variable cannot change
without a server restart, so that one restart is by design. A normal rs-drive open-loop run
ends the default-server phase. nvidia-smi logs clocks, temperature and power every 500 ms
throughout. Writes out/diag-ka/ (gitignored) and prints the summary.
"""

import json
import os
import subprocess
import sys
import time
from datetime import datetime

import diag
import run as R

HERE = os.path.dirname(os.path.abspath(__file__))
KA = os.path.join(HERE, "out", "diag-ka")
SMI = ["nvidia-smi", "--query-gpu=timestamp,clocks.sm,temperature.gpu,power.draw,clocks_event_reasons.active",
       "--format=csv,noheader,nounits", "-lms", "500"]

_orig = R.configs


def configs(run_dir):
    c = _orig(run_dir)
    for n in (20, 100):
        d = dict(c["D2"])
        cmd = list(d["cmd"])
        cmd[cmd.index("-c") + 1] = str(n)
        d["cmd"], d["label"] = cmd, f"oha -c {n} --latency-correction"
        c[f"D{n}"] = d
    return c


R.configs = configs


def gpu_window(path, t0_ns, t1_ns):
    rows = []
    for line in open(path):
        f = [x.strip() for x in line.split(",")]
        if len(f) < 5 or not f[1].replace(".", "").isdigit():
            continue
        t = datetime.strptime(f[0], "%Y/%m/%d %H:%M:%S.%f").timestamp() * 1e9  # local time, as nvidia-smi prints it
        if t0_ns <= t <= t1_ns:
            rows.append((float(f[1]), float(f[2]), float(f[3]), f[4]))
    if not rows:
        return None
    clk = [r[0] for r in rows]
    return dict(samples=len(rows), sm_mhz_mean=sum(clk) / len(clk), sm_mhz_min=min(clk), sm_mhz_max=max(clk),
                temp_c_max=max(r[1] for r in rows), power_w_mean=sum(r[2] for r in rows) / len(rows),
                reasons=sorted({r[3] for r in rows}))


def ss_facts(path, rec):
    ts, tc = rec["freeze_stop_ns"], rec["freeze_cont_ns"]
    estab_before = estab_after = None
    recvq_max = 0
    for ln in open(path):
        r = json.loads(ln)
        rel = r["t"] - tc
        srv = [row.split() for row in r["ss"] if row.split()[3].endswith(f":{R.PORT}") and row.split()[0] == "ESTAB"]
        if r["t"] < ts:
            estab_before = len(srv)
        if ts <= r["t"] <= tc:
            recvq_max = max(recvq_max, sum(1 for f in srv if int(f[1]) > 0))
        if estab_after is None and rel >= 0.5e9:
            estab_after = len(srv)
    return dict(server_estab_before_stop=estab_before, server_estab_0p5s_after_cont=estab_after, recvq_nonzero_max_in_freeze=recvq_max)


def one(server, cid, tag, summary, smi_path):
    time.sleep(5)
    ok, why, s0_pre = R.health_ok(server)
    restarted = None
    if not ok:  # the only allowed restart
        restarted = why
        server.stop()
        server.start()
        s0_pre = server.baseline_ms
    t_start = time.time_ns()
    rec, cnt, slog, d = diag.run_step(server, cid, tag, True)
    t_end = time.time_ns()
    rep = rec["reported"] or {}
    oj = json.load(open(f"{d}/stdout.txt")) if cid != "A" else None
    out = dict(tag=tag, config=cid, tool=rec["tool"], command=" ".join(map(str, rec["command"])), restarted_before=restarted,
               s0_pre_ms=s0_pre, s0_post_ms=rec.get("s0_post_ms"), freeze_s=rec["freeze_s"],
               freeze_after_first_s=rec["freeze_after_first_s"], reported=rep,
               counters={k: v for k, v in cnt.items() if k in ("Tcp.EstabResets", "Tcp.OutRsts", "TcpExt.TCPAbortOnClose",
                                                            "TcpExt.TCPAbortOnData", "Tcp.AttemptFails")},
               ss=ss_facts(f"{d}/ss.jsonl", rec), gpu=gpu_window(smi_path, t_start, t_end),
               server_posts=rec.get("server_posts"))
    if oj:
        out["errorDistribution"] = oj["errorDistribution"]
        out["statusCodeDistribution"] = oj["statusCodeDistribution"]
        out["connection_errors"] = oj["errorDistribution"].get("connection error", 0)
        out["oha_p99_ms"] = oj["latencyPercentiles"]["p99"] * 1e3
        out["oha_p50_ms"] = oj["latencyPercentiles"]["p50"] * 1e3
    summary.append(out)
    json.dump(summary, open(os.path.join(KA, "summary.json"), "w"), indent=1)
    print(json.dumps({k: out[k] for k in out if k not in ("command",)}, default=str), flush=True)


def main():
    os.makedirs(KA, exist_ok=True)
    diag.DIAG = os.path.join(KA, "d")
    smi_path = os.path.join(KA, "gpu.csv")
    smi = subprocess.Popen(SMI, stdout=open(smi_path, "w"), stderr=subprocess.DEVNULL, start_new_session=True)
    summary = []
    try:
        server = diag.start_server(None)
        for cid, tag in (("D20", "c20"), ("D100", "c100"), ("D2", "c50-a"), ("D2", "c50-b"), ("A", "normal-A")):
            one(server, cid, tag, summary, smi_path)
        server.stop()
        time.sleep(5)
        server = diag.start_server(300)
        one(server, "D2", "c50-ka300", summary, smi_path)
        server.stop()
    finally:
        smi.terminate()
    print("done")


if __name__ == "__main__":
    sys.exit(main())
