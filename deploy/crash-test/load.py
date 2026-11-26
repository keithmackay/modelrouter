#!/usr/bin/env python3
"""Steady load for the crash test: N workers send chat completions through
the balancer for D seconds, a share of them streamed, and every outcome is
classified. Writes a JSON summary to stdout; each successful request's
correlation id goes to --ok-ids so the driver can match ledger rows.

    load.py --url http://127.0.0.1:18080 --key mr-... --secs 60 --workers 16 \
            --stream-share 0.3 --run crash-1 --ok-ids ok.txt
"""
import argparse
import http.client
import json
import random
import threading
import time
from collections import Counter
from urllib.parse import urlparse


def one(u, key, stream, cid):
    body = json.dumps({
        "model": "mock-model",
        "stream": stream,
        "messages": [{"role": "user", "content": f"hello {cid} {random.random()}"}],
    })
    conn = http.client.HTTPConnection(u.hostname, u.port, timeout=30)
    try:
        conn.request("POST", "/v1/chat/completions", body, {
            "authorization": f"Bearer {key}",
            "content-type": "application/json",
            "connection": "close",
            "x-attribution-correlation-id": cid,
        })
        resp = conn.getresponse()
        data = resp.read()
        if resp.status != 200:
            return f"http_{resp.status}"
        if stream:
            return "ok" if b"[DONE]" in data else "stream_cut"
        json.loads(data)
        return "ok"
    except http.client.IncompleteRead:
        return "stream_cut" if stream else "truncated"
    except (ConnectionError, http.client.RemoteDisconnected) as e:
        return f"conn_{type(e).__name__}"
    except TimeoutError:
        return "timeout"
    except Exception as e:  # noqa: BLE001 - every failure is counted, not raised
        return f"err_{type(e).__name__}"
    finally:
        conn.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--secs", type=float, default=60)
    ap.add_argument("--workers", type=int, default=16)
    ap.add_argument("--stream-share", type=float, default=0.3)
    ap.add_argument("--run", default="crash")
    ap.add_argument("--ok-ids", required=True)
    args = ap.parse_args()
    u = urlparse(args.url)

    lock = threading.Lock()
    outcomes = Counter()
    by_kind = {"stream": Counter(), "plain": Counter()}
    failures = []  # (t, kind, outcome)
    ok_ids = []
    seq = iter(range(10**9))
    t0 = time.monotonic()
    deadline = t0 + args.secs

    def worker():
        while time.monotonic() < deadline:
            with lock:
                n = next(seq)
            stream = random.random() < args.stream_share
            cid = f"{args.run}-{n}"
            out = one(u, args.key, stream, cid)
            kind = "stream" if stream else "plain"
            with lock:
                outcomes[out] += 1
                by_kind[kind][out] += 1
                if out == "ok":
                    ok_ids.append(cid)
                else:
                    failures.append((round(time.monotonic() - t0, 2), kind, out))

    threads = [threading.Thread(target=worker) for _ in range(args.workers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    with open(args.ok_ids, "w") as f:
        f.write("\n".join(ok_ids) + ("\n" if ok_ids else ""))
    total = sum(outcomes.values())
    print(json.dumps({
        "total": total,
        "ok": outcomes["ok"],
        "failed": total - outcomes["ok"],
        "outcomes": dict(outcomes),
        "stream": dict(by_kind["stream"]),
        "plain": dict(by_kind["plain"]),
        "failures": failures[:50],
    }, indent=2))


if __name__ == "__main__":
    main()
