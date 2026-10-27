#!/usr/bin/env python3
"""Analytic expectation for a request schedule hitting a server that freezes.

No benchmark tool is involved: the inputs are the schedule (a request every
PERIOD seconds from an origin), the freeze interval [F0, F1] that the
orchestrator logged, the baseline service time s0 and the resume penalty r.

Assumptions
-----------
* The schedule is perfectly periodic: request i is intended at ORIGIN + i*PERIOD.
* The server has no queueing of its own at this load: it serves one request in
  s0 seconds and the offered load (10 req/s * ~91 ms) is far below its batch
  capacity, so concurrent requests do not slow each other down.
* During [F0, F1] the server makes no progress at all (SIGSTOP of its whole
  process group). A request that arrives then is answered after F1.
* s0 and r come from calibration.json (measured in STEP 0 of this run, three
  freezes). r is the extra time the request in flight at the freeze took, over
  and above "time until F1" + s0.

Two models
----------
unlimited  Every request is sent at its intended time and runs concurrently.
           latency = (F1 - t) + s0 + r   if F0 <= t <= F1 (t = intended time)
           latency = s0                  otherwise.
           Applies to: rs-drive open, and any tool whose docs / --help show it
           has no concurrency cap that this load can reach (see README).

fifo       One connection (a FIFO single server). A request is sent at
           max(intended time, completion of the previous request). The server
           takes s0 per request and makes no progress during [F0, F1]; the one
           request that is in service when the freeze ends carries r.
           Two latencies come out of the simulation:
             service_latency  = done - send time        (what an uncorrected
                                                          tool measures)
             intended_latency = done - intended time    (what a tool measures
                                                          when it corrects)
           Applies to: oha -c 1, rs-drive closed.
           Utilisation is s0/PERIOD (about 91%), so after the freeze the backlog
           drains at about PERIOD - s0 (about 9 ms) per request; simulate()
           shows that and `drain_requests` counts how many requests stay
           above 2*s0 of intended latency.

Phase averaging: when the schedule origin is not known, the freeze start
relative to the origin is FREEZE_AFTER + phi, with phi taken uniformly over one
period (STEP_MS steps), and the statistics are averaged over phi.

All times are seconds unless a name ends in _ms. Percentiles are nearest rank:
the ceil(q*n)-th smallest value.
"""

import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PERIOD = 0.1
RATE = 10
N = 250
FREEZE_AFTER = 8.0  # the orchestrator freezes 8 s after the first request
STEP_MS = 1


def load_calibration():
    with open(os.path.join(HERE, "calibration.json")) as f:
        return json.load(f)


def nearest_rank(values, q):
    s = sorted(values)
    return s[max(1, math.ceil(q * len(s))) - 1]


def stats(latencies_s):
    """p50, p90, p99, max in milliseconds."""
    return {
        "p50": nearest_rank(latencies_s, 0.50) * 1e3,
        "p90": nearest_rank(latencies_s, 0.90) * 1e3,
        "p99": nearest_rank(latencies_s, 0.99) * 1e3,
        "max": max(latencies_s) * 1e3,
    }


def unlimited(origin, f0, f1, s0, r, n=N, period=PERIOD):
    out = []
    for i in range(n):
        t = origin + i * period
        out.append((f1 - t) + s0 + r if f0 <= t <= f1 else s0)
    return out


def simulate_fifo(origin, f0, f1, s0, r, n=N, period=PERIOD):
    """Return (service_latencies, intended_latencies) for a single FIFO server."""
    service, intended = [], []
    free_at = origin
    for i in range(n):
        t = origin + i * period
        send = max(t, free_at)
        start = send
        done = start + s0
        if start >= f0 and start < f1:  # arrives while frozen: starts at resume
            start = f1
            done = start + s0 + r
        elif start < f0 < done:  # freeze begins during service: it resumes at f1
            done = done + (f1 - f0) + r
        free_at = done
        service.append(done - send)
        intended.append(done - t)
    return service, intended


def drain_requests(intended, s0, period=PERIOD):
    """Requests after the first slow one whose intended latency is above 2*s0."""
    slow = [i for i, v in enumerate(intended) if v > 2 * s0]
    return len(slow)


def phase_average(model, f0_rel, f1_rel, s0, r, n=N, period=PERIOD):
    """Mean of the statistics over origin phases; also the min and max of p99."""
    runs = []
    for k in range(0, int(period * 1e3), STEP_MS):
        origin = 0.0
        shift = k / 1e3
        f0, f1 = f0_rel + shift, f1_rel + shift
        runs.append(model(origin, f0, f1, s0, r, n, period))
    return runs


def avg(dicts):
    keys = dicts[0].keys()
    return {k: sum(d[k] for d in dicts) / len(dicts) for k in keys}


def expected(model, origin, f0, f1, s0, r, n=N):
    """Exact case: the origin is known. `model` is 'unlimited' or 'fifo'."""
    if model == "unlimited":
        return {"latency": stats(unlimited(origin, f0, f1, s0, r, n))}
    service, intended = simulate_fifo(origin, f0, f1, s0, r, n)
    return {"service_latency": stats(service), "intended_latency": stats(intended)}


def expected_phase_averaged(model, freeze_len, s0, r, n=N, freeze_after=FREEZE_AFTER):
    """Phase-averaged case: the origin is unknown, F0 - origin = freeze_after + phi.

    freeze_after is the logged STOP time minus the first request the server logged
    (8.0 s nominally; build_results passes the logged value for each run)."""
    f0_rel, f1_rel = freeze_after, freeze_after + freeze_len
    if model == "unlimited":
        runs = phase_average(unlimited, f0_rel, f1_rel, s0, r, n)
        return {"latency": avg([stats(x) for x in runs])}
    runs = phase_average(lambda *a: simulate_fifo(*a), f0_rel, f1_rel, s0, r, n)
    return {
        "service_latency": avg([stats(x[0]) for x in runs]),
        "intended_latency": avg([stats(x[1]) for x in runs]),
    }


def main():
    cal = load_calibration()
    s0, r, fl = cal["s0_ms"] / 1e3, cal["r_ms"] / 1e3, cal["freeze_s"]
    print(f"s0 = {s0 * 1e3:.1f} ms, r = {r * 1e3:.1f} ms, freeze = {fl:.3f} s")
    print(f"r source: {cal['r_source']}")
    for model in ("unlimited", "fifo"):
        print(f"\nphase-averaged, model={model}")
        for name, st in expected_phase_averaged(model, fl, s0, r).items():
            print("  %-17s" % name, "  ".join(f"{k}={v:8.1f} ms" for k, v in st.items()))
    service, intended = simulate_fifo(0.0, FREEZE_AFTER, FREEZE_AFTER + fl, s0, r)
    print("\nFIFO backlog after the freeze (origin 0, F0 = 8.0 s), intended latency per request:")
    first = int(FREEZE_AFTER / PERIOD)
    for i in range(first, first + 40, 4):
        print(f"  i={i:3d}  intended {intended[i] * 1e3:7.1f} ms   service {service[i] * 1e3:7.1f} ms")
    print(f"  requests with intended latency > 2*s0: {drain_requests(intended, s0)}")
    drained = [i for i in range(first, N) if intended[i] <= 2 * s0]
    if drained:
        print(f"  first request back under 2*s0 after the freeze: i={drained[0]}")


if __name__ == "__main__":
    sys.exit(main())
