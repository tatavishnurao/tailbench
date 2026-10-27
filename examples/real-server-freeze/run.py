#!/usr/bin/env python3
"""Freeze a real vLLM server for 2.0 s while each load tool drives it.

Standard library only. For every configuration, two repetitions, one run at a
time: start the tool, wait for its first POST /v1/completions in the server
log, SIGSTOP the server's process group 8 s later, SIGCONT it 2.0 s after
that, and log the wall-clock time (unix ns) before STOP and after CONT. That
log is the independent truth; no tool controls it.

    python3 run.py            # all configurations, 2 repetitions
    python3 run.py --only A   # one configuration (for trying it out)
    python3 run.py --reps 1

Writes out/ (gitignored: raw tool output, commands, server log) and
results.json (see build_results).
"""

import argparse
import glob
import json
import os
import signal
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "out")
HOME = os.path.expanduser("~")
TOOLS = os.path.join(HOME, "audit-tools")
MODEL = "Qwen/Qwen2.5-0.5B-Instruct"
PORT = 18000
BASE = f"http://127.0.0.1:{PORT}"
RATE = 10
DURATION = 25
FREEZE_AFTER = 8.0
FREEZE_LEN = 2.0
IDLE_GAP = 10
REPS = 2

# The server's environment is explicit: no /mnt/* (Windows) entries on PATH.
# See "WSL notes" in the README.
SERVER_ENV = {
    "PATH": f"{TOOLS}/vllm-venv/bin:/usr/local/cuda/bin:/usr/bin:/bin",
    "CUDA_HOME": "/usr/local/cuda",
    "HOME": HOME,
}
SERVER_CMD = [
    f"{TOOLS}/vllm-venv/bin/vllm", "serve", MODEL, "--max-model-len", "2048",
    "--gpu-memory-utilization", "0.5", "--port", str(PORT),
]
BODY = json.dumps({"model": MODEL, "prompt": "Hello", "max_tokens": 16, "temperature": 0})
DRIVER = os.path.join(HERE, "driver", "target", "release", "rs-drive")


def log(msg):
    print(time.strftime("%H:%M:%S"), msg, flush=True)


# ---------------------------------------------------------------- server ----

class Server:
    def __init__(self):
        self.proc = None
        self.restarts = []

    def start(self):
        path = os.path.join(OUT, "server.log")
        self.logf = open(path, "ab")
        self.proc = subprocess.Popen(
            SERVER_CMD, env=SERVER_ENV, stdin=subprocess.DEVNULL, stdout=self.logf,
            stderr=subprocess.STDOUT, start_new_session=True,  # its own process group
        )
        self.pgid = os.getpgid(self.proc.pid)
        deadline = time.time() + 400
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise SystemExit(f"vllm exited with {self.proc.returncode}; see out/server.log")
            try:
                urllib.request.urlopen(BASE + "/v1/models", timeout=2).read()
                break
            except Exception:
                time.sleep(2)
        else:
            raise SystemExit("vllm did not answer /v1/models in 400 s")
        for _ in range(20):
            complete()
        self.baseline_ms = median([complete() for _ in range(10)])
        log(f"server up, pgid {self.pgid}, baseline {self.baseline_ms:.1f} ms")

    def stop(self):
        if self.proc and self.proc.poll() is None:
            os.killpg(self.pgid, signal.SIGTERM)
            try:
                self.proc.wait(30)
            except subprocess.TimeoutExpired:
                os.killpg(self.pgid, signal.SIGKILL)
                self.proc.wait()
        time.sleep(3)

    def log_size(self):
        return os.path.getsize(os.path.join(OUT, "server.log"))

    def posts_since(self, offset):
        with open(os.path.join(OUT, "server.log"), "rb") as f:
            f.seek(offset)
            return f.read().count(b"POST /v1/completions")


def median(xs):
    s = sorted(xs)
    return s[len(s) // 2]


def complete():
    """One /v1/completions request; latency in ms."""
    t = time.perf_counter()
    req = urllib.request.Request(BASE + "/v1/completions", BODY.encode(), {"Content-Type": "application/json"})
    urllib.request.urlopen(req, timeout=60).read()
    return (time.perf_counter() - t) * 1e3


def health_ok(server):
    try:
        urllib.request.urlopen(BASE + "/health", timeout=5).read()
        m = median([complete() for _ in range(5)])
    except Exception as e:
        return False, f"health failed: {e}", None
    if m >= 3 * server.baseline_ms:
        return False, f"median {m:.1f} ms >= 3 x baseline {server.baseline_ms:.1f} ms", m
    return True, f"median {m:.1f} ms", m


# ----------------------------------------------------------- configurations -

def venv_env():
    return {"PATH": SERVER_ENV["PATH"], "HOME": HOME}


def configs(run_dir):
    """id -> dict(label, cmd, concurrency, model, latency, parse)."""
    url = f"{BASE}/v1/completions"
    oha = ["oha", "-q", str(RATE), "-z", f"{DURATION}s", "--no-tui", "--output-format", "json",
           "-m", "POST", "-d", BODY, "-H", "content-type: application/json"]
    oha = [os.path.join(HOME, ".cargo/bin/oha")] + oha[1:]
    c = {}
    c["A"] = dict(
        label="tailbench open loop (rs-drive)", cmd=[DRIVER, "--mode", "open", "--url", BASE, "--model", MODEL,
            "--rate", str(RATE), "--duration", str(DURATION), "--out", f"{run_dir}/drive"],
        concurrency="unbounded (tailbench open_loop: one task per request)", model="unlimited", latency="latency",
        metric="open_loop latency = t_done - t_intended", parse="drive")
    c["B"] = dict(
        label="tailbench closed loop (rs-drive)", cmd=[DRIVER, "--mode", "closed", "--url", BASE, "--model", MODEL,
            "--rate", str(RATE), "--duration", str(DURATION), "--out", f"{run_dir}/drive"],
        concurrency="1 (tailbench closed_loop: at most one request in flight)", model="fifo", latency="service_latency",
        metric="closed_loop latency = t_done - t_sent", parse="drive")
    c["C"] = dict(label="oha -c 1", cmd=oha + ["-c", "1", url], concurrency="1 (-c 1)",
                  model="fifo", latency="service_latency", metric="latencyPercentiles p50/p99, summary.slowest", parse="oha")
    c["D"] = dict(label="oha -c 1 --latency-correction", cmd=oha + ["-c", "1", "--latency-correction", url],
                  concurrency="1 (-c 1)", model="fifo", latency="intended_latency",
                  metric="latencyPercentiles p50/p99, summary.slowest", parse="oha")
    c["C2"] = dict(label="oha -c 50", cmd=oha + ["-c", "50", url], concurrency="50 (-c 50; at most ~21 in flight here)",
                   model="unlimited", latency="latency", metric="latencyPercentiles p50/p99, summary.slowest", parse="oha")
    c["D2"] = dict(label="oha -c 50 --latency-correction", cmd=oha + ["-c", "50", "--latency-correction", url],
                   concurrency="50 (-c 50; at most ~21 in flight here)", model="unlimited", latency="latency",
                   metric="latencyPercentiles p50/p99, summary.slowest", parse="oha")
    vb = os.path.join(TOOLS, "vllm-venv/bin/vllm")
    c["E"] = dict(
        label="vllm bench serve", cmd=[vb, "bench", "serve", "--backend", "openai", "--base-url", BASE,
            "--endpoint", "/v1/completions", "--model", MODEL, "--tokenizer", MODEL, "--dataset-name", "random",
            "--random-input-len", "64", "--random-output-len", "16", "--ignore-eos",
            "--num-prompts", str(RATE * DURATION), "--request-rate", str(RATE), "--burstiness", "inf",
            "--seed", "42", "--request-id-prefix", "", "--percentile-metrics", "e2el",
            "--metric-percentiles", "50,90,99,100", "--save-result", "--result-dir", run_dir,
            "--result-filename", "vllm.json", "--disable-tqdm"],
        concurrency="no cap (--max-concurrency default None)", model="unlimited", latency="latency",
        metric="E2EL: p50_e2el_ms, p99_e2el_ms, p100_e2el_ms", parse="vllm")
    gl = os.path.join(TOOLS, "guidellm-venv/bin/guidellm")
    backend = json.dumps({"kind": "openai_http", "target": BASE, "model": MODEL,
                          "request_format": "/v1/completions", "stream": False})
    c["F"] = dict(
        label="GuideLLM constant", cmd=[gl, "run", "--backend", backend, "--profile", f"kind=constant,rate={RATE}",
            "--constraint", f"kind=max_duration,seconds={DURATION}",
            "--data", "kind=synthetic_text,prompt_tokens=64,output_tokens=16",
            "--tokenizer", f"kind=huggingface_auto,model={MODEL}", "--seed", "kind=static,value=42",
            "--output", f"kind=json,path={run_dir}/guidellm.json", "--disable-console-interactive"],
        concurrency="max_concurrency 10240 (the recorded strategy config; --profile max_concurrency unset)",
        model="unlimited", latency="latency",
        metric="metrics.request_latency.successful: percentiles.p50, percentiles.p99, max", parse="guidellm")
    ap = os.path.join(TOOLS, "aiperf-venv/bin/aiperf")
    c["G"] = dict(
        label="AIPerf constant", cmd=[ap, "profile", "--model", MODEL, "--url", BASE, "--endpoint-type", "completions",
            "--request-rate", str(RATE), "--arrival-pattern", "constant", "--benchmark-duration", str(DURATION),
            "--prompt-input-tokens-mean", "64", "--prompt-output-tokens-mean", "16", "--tokenizer", MODEL,
            "--ui-type", "none", "--artifact-dir", f"{run_dir}/aiperf"],
        concurrency="no --concurrency set (the recorded phase config has no concurrency key)",
        model="unlimited", latency="latency", metric="request_latency: p50, p99, max", parse="aiperf")
    return c


def version(cfg_id):
    cmd = {"A": None, "B": None}.get(cfg_id, 1)
    if cmd is None:
        return "tailbench (path ../../..)"
    tool = {"C": "oha", "D": "oha", "C2": "oha", "D2": "oha", "E": "vllm", "F": "guidellm", "G": "aiperf"}[cfg_id]
    exe = {"oha": os.path.join(HOME, ".cargo/bin/oha"), "vllm": f"{TOOLS}/vllm-venv/bin/vllm",
           "guidellm": f"{TOOLS}/guidellm-venv/bin/guidellm", "aiperf": f"{TOOLS}/aiperf-venv/bin/aiperf"}[tool]
    out = subprocess.run([exe, "--version"], capture_output=True, text=True, env=venv_env()).stdout.strip()
    return f"{tool} {out.splitlines()[-1] if out else '?'}"


# ------------------------------------------------------------------ parsing -

def parse_tool(kind, run_dir, stdout_path):
    ms = 1e3
    if kind == "drive":
        d = json.loads(open(stdout_path).read().strip().splitlines()[-1])
        return dict(p50=d["p50"], p99=d["p99"], max=d["max"], n=d["n"], errors=d["errors"],
                    origin_unix_ns=d["origin_unix_ns"])
    if kind == "oha":
        d = json.load(open(stdout_path))
        n = sum(d["statusCodeDistribution"].values())
        return dict(p50=d["latencyPercentiles"]["p50"] * ms, p99=d["latencyPercentiles"]["p99"] * ms,
                    max=d["summary"]["slowest"] * ms, n=n, errors=sum(d["errorDistribution"].values()))
    if kind == "vllm":
        d = json.load(open(f"{run_dir}/vllm.json"))
        return dict(p50=d["p50_e2el_ms"], p99=d["p99_e2el_ms"], max=d["p100_e2el_ms"], n=d["completed"],
                    errors=d["failed"], duration_s=d["duration"])
    if kind == "guidellm":
        b = json.load(open(f"{run_dir}/guidellm.json"))["benchmarks"][0]
        r = b["metrics"]["request_latency"]["successful"]
        s = b["metrics"]["request_scheduled_latency"]["successful"]
        return dict(p50=r["percentiles"]["p50"] * ms, p99=r["percentiles"]["p99"] * ms, max=r["max"] * ms,
                    n=r["count"], errors=b["metrics"]["request_latency"]["errored"]["count"],
                    scheduled_p99=s["percentiles"]["p99"] * ms, scheduled_max=s["max"] * ms,
                    scheduled_metric="request_scheduled_latency")
    if kind == "aiperf":
        d = json.load(open(f"{run_dir}/aiperf/profile_export_aiperf.json"))
        r = d["request_latency"]
        return dict(p50=r["p50"], p99=r["p99"], max=r["max"], n=int(d["request_count"]["avg"]),
                    errors=int(d.get("error_summary") and len(d["error_summary"]) or 0))
    raise ValueError(kind)


# --------------------------------------------------------------- one run ----

def one_run(server, cfg_id, cfg, rep, s0_pre_ms):
    run_id = f"{cfg_id}-rep{rep}"
    run_dir = os.path.join(OUT, "runs", run_id)
    os.makedirs(run_dir, exist_ok=True)
    cfg = configs(run_dir)[cfg_id]
    cmd = cfg["cmd"]
    open(f"{run_dir}/command.txt", "w").write(" ".join(repr(c) if " " in c or not c else c for c in cmd) + "\n")
    stdout_path = f"{run_dir}/stdout.txt"
    offset = server.log_size()
    t_start = time.time_ns()
    with open(stdout_path, "wb") as so, open(f"{run_dir}/stderr.txt", "wb") as se:
        proc = subprocess.Popen(cmd, stdout=so, stderr=se, env=venv_env(), stdin=subprocess.DEVNULL,
                                start_new_session=True)
        # The tool's first request: the first new POST /v1/completions line. uvicorn
        # writes it when the response goes out, so this is the first request's
        # completion; the first request was sent about one service time earlier.
        t_first = None
        deadline = time.time() + 180
        while time.time() < deadline and proc.poll() is None:
            if server.posts_since(offset):
                t_first = time.time_ns()
                break
            time.sleep(0.02)
        rec = dict(id=run_id, config=cfg_id, rep=rep, tool=cfg["label"], command=cmd, concurrency=cfg["concurrency"],
                   metric=cfg["metric"], expected_model=cfg["model"], expected_latency=cfg["latency"],
                   server_restarted_before=False)
        if t_first is None:
            proc.kill()
            rec["error"] = "no POST /v1/completions seen"
            return rec
        wake = time.monotonic() + (FREEZE_AFTER - (time.time_ns() - t_first) / 1e9)
        time.sleep(max(0, wake - time.monotonic()))
        t_stop = time.time_ns()
        os.killpg(server.pgid, signal.SIGSTOP)
        time.sleep(FREEZE_LEN)
        os.killpg(server.pgid, signal.SIGCONT)
        t_cont = time.time_ns()
        try:
            proc.wait(timeout=150)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            rec["error"] = "tool did not exit within 150 s of the freeze; killed"
    t_end = time.time_ns()
    # Tool-independent service time after the run (the server is idle again).
    time.sleep(2)
    rec["s0_pre_ms"] = s0_pre_ms
    rec["s0_post_ms"] = median([complete() for _ in range(5)])
    rec.update(t_tool_start_ns=t_start, t_first_post_ns=t_first, freeze_stop_ns=t_stop, freeze_cont_ns=t_cont,
               freeze_s=(t_cont - t_stop) / 1e9, freeze_after_first_s=(t_stop - t_first) / 1e9,
               exit_code=proc.returncode, wall_s=(t_end - t_start) / 1e9,
               server_posts=server.posts_since(offset))
    try:
        rec["reported"] = parse_tool(cfg["parse"], run_dir, stdout_path)
    except Exception as e:
        rec["error"] = (rec.get("error", "") + f" parse failed: {type(e).__name__}: {e}").strip()
    return rec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", help="comma-separated configuration ids")
    ap.add_argument("--reps", type=int, default=REPS)
    args = ap.parse_args()
    os.makedirs(os.path.join(OUT, "runs"), exist_ok=True)
    if not os.path.exists(DRIVER):
        sys.exit("build the driver first: cargo build --release in driver/")
    ids = [i for i in configs("/tmp") if not args.only or i in args.only.split(",")]
    started = time.time()
    server = Server()
    server.start()
    runs, versions = [], {i: version(i) for i in ids}
    try:
        for rep in range(1, args.reps + 1):
            for cid in ids:
                log(f"idle {IDLE_GAP} s")
                time.sleep(IDLE_GAP)
                ok, why, s0_pre = health_ok(server)
                restarted = None
                if not ok:
                    restarted = why
                    log(f"check failed ({why}); restarting the server")
                    server.stop()
                    server.restarts.append(dict(before=f"{cid}-rep{rep}", reason=why))
                    server.start()
                    s0_pre = server.baseline_ms
                log(f"run {cid} rep {rep}: {configs('/tmp')[cid]['label']}")
                rec = one_run(server, cid, None, rep, s0_pre)
                rec["server_restarted_before"] = restarted
                rec["version"] = versions[cid]
                runs.append(rec)
                log(f"  -> {rec.get('reported') or rec.get('error')}  freeze {rec.get('freeze_s')}")
                json.dump(runs, open(os.path.join(OUT, "runs.json"), "w"), indent=1)
    finally:
        server.stop()
    meta = dict(model=MODEL, vllm=subprocess.run([f"{TOOLS}/vllm-venv/bin/vllm", "--version"], capture_output=True,
                text=True, env=venv_env()).stdout.strip().splitlines()[-1],
                gpu=subprocess.run(["nvidia-smi", "--query-gpu=name,memory.total,driver_version", "--format=csv,noheader"],
                capture_output=True, text=True).stdout.strip(),
                rate=RATE, duration_s=DURATION, freeze_s=FREEZE_LEN, freeze_after_first_s=FREEZE_AFTER, reps=args.reps,
                baseline_ms=server.baseline_ms, server_restarts=server.restarts,
                total_runtime_s=time.time() - started)
    json.dump(dict(meta=meta, runs=runs), open(os.path.join(OUT, "runs-raw.json"), "w"), indent=1)
    log(f"done in {meta['total_runtime_s'] / 60:.1f} min; now run build_results.py")


if __name__ == "__main__":
    main()
