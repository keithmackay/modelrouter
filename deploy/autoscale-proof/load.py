#!/usr/bin/env python3
"""Load client for the autoscaling proof.

Same request shape as deploy/crash-test/load.py, plus per-request latency
so a scale-up from one pod can be described while new pods become Ready.
Writes one JSON object to stdout.
"""
import argparse
import http.client
import json
import random
import threading
import time
from collections import Counter
from urllib.parse import urlparse


def percentile(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    if len(xs) == 1:
        return xs[0]
    i = (len(xs) - 1) * p
    lo = int(i)
    hi = min(lo + 1, len(xs) - 1)
    frac = i - lo
    return xs[lo] * (1 - frac) + xs[hi] * frac


def one(u, key, stream, cid):
    body = json.dumps({
        "model": "mock-model",
        "stream": stream,
        "messages": [{"role": "user", "content": f"hello {cid} {random.random()}"}],
    }).encode()
    conn = http.client.HTTPConnection(u.hostname, u.port, timeout=30)
    t0 = time.monotonic()
    try:
        conn.request("POST", "/v1/chat/completions", body, {
            "authorization": f"Bearer {key}",
            "content-type": "application/json",
            "connection": "close",
            "x-attribution-correlation-id": cid,
        })
        resp = conn.getresponse()
        data = resp.read()
        ms = (time.monotonic() - t0) * 1000
        if resp.status != 200:
            return f"http_{resp.status}", ms
        if stream:
            return ("ok" if b"[DONE]" in data else "stream_cut"), ms
        json.loads(data)
        return "ok", ms
    except http.client.IncompleteRead:
        return ("stream_cut" if stream else "truncated"), (time.monotonic() - t0) * 1000
    except (ConnectionError, http.client.RemoteDisconnected) as e:
        return f"conn_{type(e).__name__}", (time.monotonic() - t0) * 1000
    except TimeoutError:
        return "timeout", (time.monotonic() - t0) * 1000
    except Exception as e:  # noqa: BLE001 - every failure is counted, not raised
        return f"err_{type(e).__name__}", (time.monotonic() - t0) * 1000
    finally:
        conn.close()


def lat_summary(samples):
    xs = [ms for ms in samples]
    return {
        "n": len(xs),
        "p50_ms": percentile(xs, 0.50),
        "p95_ms": percentile(xs, 0.95),
        "max_ms": max(xs) if xs else None,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--secs", type=float, default=60)
    ap.add_argument("--workers", type=int, default=16)
    ap.add_argument("--stream-share", type=float, default=0.0)
    ap.add_argument("--run", default="proof")
    ap.add_argument("--ok-ids", default="/tmp/ok.txt")
    ap.add_argument("--bucket-secs", type=float, default=10)
    args = ap.parse_args()
    u = urlparse(args.url)

    lock = threading.Lock()
    outcomes = Counter()
    by_kind = {"stream": Counter(), "plain": Counter()}
    failures = []
    ok_ids = []
    samples = []  # (t_rel, kind, outcome, ms)
    seq = iter(range(10**9))
    wall0 = time.time()
    t0 = time.monotonic()
    deadline = t0 + args.secs

    def worker():
        while time.monotonic() < deadline:
            with lock:
                n = next(seq)
            stream = random.random() < args.stream_share
            cid = f"{args.run}-{n}"
            out, ms = one(u, args.key, stream, cid)
            kind = "stream" if stream else "plain"
            rel = time.monotonic() - t0
            with lock:
                outcomes[out] += 1
                by_kind[kind][out] += 1
                samples.append((rel, kind, out, ms))
                if out == "ok":
                    ok_ids.append(cid)
                else:
                    failures.append((round(rel, 2), kind, out, round(ms, 1)))

    threads = [threading.Thread(target=worker) for _ in range(args.workers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    with open(args.ok_ids, "w") as f:
        f.write("\n".join(ok_ids) + ("\n" if ok_ids else ""))

    buckets = []
    if samples:
        end = max(s[0] for s in samples)
        edge = 0.0
        while edge <= end + 1e-6:
            chunk = [s for s in samples if edge <= s[0] < edge + args.bucket_secs]
            oks = [s[3] for s in chunk if s[2] == "ok"]
            buckets.append({
                "start_s": round(edge, 1),
                "n": len(chunk),
                "ok": len(oks),
                "failed": len(chunk) - len(oks),
                **{k: (round(v, 1) if isinstance(v, float) else v) for k, v in lat_summary(oks).items() if k != "n"},
            })
            edge += args.bucket_secs

    total = sum(outcomes.values())
    ok_ms = [s[3] for s in samples if s[2] == "ok"]
    print(json.dumps({
        "run": args.run,
        "started_at": wall0,
        "ended_at": time.time(),
        "total": total,
        "ok": outcomes["ok"],
        "failed": total - outcomes["ok"],
        "outcomes": dict(outcomes),
        "stream": dict(by_kind["stream"]),
        "plain": dict(by_kind["plain"]),
        "latency_ok_ms": {k: (round(v, 1) if isinstance(v, float) else v) for k, v in lat_summary(ok_ms).items()},
        "buckets": buckets,
        "failures": failures[:50],
    }, indent=2))


if __name__ == "__main__":
    main()
