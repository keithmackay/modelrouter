#!/usr/bin/env python3
"""A small HTTP/1.1 load balancer for the crash test, standing in for a
Kubernetes Service plus (optionally) an ingress that retries.

* Round-robin over the backends that are in rotation.
* A readiness checker polls each backend's /health/ready every second and
  takes it out of rotation after 3 failures, back in after 1 success: the
  same shape as a readinessProbe with periodSeconds 1, failureThreshold 3.
* With --retry, a request whose backend refused the connection, or closed
  it before sending a single response byte, is sent to the next backend (at
  most 2 more tries), like ingress-nginx `proxy_next_upstream error` or
  Envoy `retry_on: connect-failure,reset-before-request`. Nothing is retried
  once a response byte has been forwarded, so a stream cut mid-way fails.
  Without --retry it behaves like a plain Service: the client sees the error.

One request per client connection (the client must send Connection: close).
Standard library only.

    lb.py --listen 18080 --backend 127.0.0.1:18081 --backend 127.0.0.1:18082 [--retry]
"""
import argparse
import asyncio
import itertools
import sys


class Backend:
    def __init__(self, spec):
        host, port = spec.rsplit(":", 1)
        self.host, self.port = host, int(port)
        self.ready = False
        self.fails = 0

    def __str__(self):
        return f"{self.host}:{self.port}"


async def read_request(reader):
    head = await reader.readuntil(b"\r\n\r\n")
    length = 0
    for line in head.split(b"\r\n")[1:]:
        k, _, v = line.partition(b":")
        if k.strip().lower() == b"content-length":
            length = int(v.strip())
    body = await reader.readexactly(length) if length else b""
    return head + body


async def probe(b):
    try:
        r, w = await asyncio.wait_for(asyncio.open_connection(b.host, b.port), 1)
        w.write(b"GET /health/ready HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        await w.drain()
        status = await asyncio.wait_for(r.readline(), 1)
        w.close()
        return b" 200 " in status
    except (OSError, asyncio.TimeoutError, asyncio.IncompleteReadError):
        return False


async def readiness_loop(backends, log):
    while True:
        for b in backends:
            ok = await probe(b)
            if ok:
                b.fails = 0
                if not b.ready:
                    log(f"lb: {b} in rotation")
                b.ready = True
            else:
                b.fails += 1
                if b.ready and b.fails >= 3:
                    b.ready = False
                    log(f"lb: {b} out of rotation")
        await asyncio.sleep(1)


class Balancer:
    def __init__(self, backends, retry, log):
        self.backends = backends
        self.retry = retry
        self.log = log
        self.rr = itertools.count()
        self.retries = 0

    def pick(self, exclude):
        live = [b for b in self.backends if b.ready and b not in exclude]
        if not live:
            return None
        return live[next(self.rr) % len(live)]

    async def handle(self, creader, cwriter):
        try:
            request = await read_request(creader)
        except (asyncio.IncompleteReadError, asyncio.LimitOverrunError, ConnectionError):
            cwriter.close()
            return
        tried = []
        attempts = 3 if self.retry else 1
        try:
            for attempt in range(attempts):
                b = self.pick(tried)
                if b is None:
                    break
                tried.append(b)
                if attempt:
                    self.retries += 1
                try:
                    breader, bwriter = await asyncio.open_connection(b.host, b.port)
                    bwriter.write(request)
                    await bwriter.drain()
                    first = await breader.read(65536)
                except (ConnectionError, OSError):
                    continue  # refused or reset before any response byte
                if not first:
                    bwriter.close()
                    continue  # closed before any response byte
                # Committed: forward the rest, and never retry from here.
                cwriter.write(first)
                try:
                    while True:
                        await cwriter.drain()
                        chunk = await breader.read(65536)
                        if not chunk:
                            break
                        cwriter.write(chunk)
                    await cwriter.drain()
                except (ConnectionError, OSError):
                    pass  # stream cut mid-way: the client sees a truncated body
                bwriter.close()
                return
            cwriter.write(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            await cwriter.drain()
        except (ConnectionError, OSError):
            pass
        finally:
            cwriter.close()


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int, required=True)
    ap.add_argument("--backend", action="append", required=True)
    ap.add_argument("--retry", action="store_true")
    args = ap.parse_args()

    def log(msg):
        print(msg, flush=True)

    backends = [Backend(s) for s in args.backend]
    lb = Balancer(backends, args.retry, log)
    asyncio.create_task(readiness_loop(backends, log))
    server = await asyncio.start_server(lb.handle, "127.0.0.1", args.listen, limit=1 << 20)
    log(f"lb: listening on {args.listen} retry={args.retry}")

    async def report():
        while True:
            await asyncio.sleep(5)
            log(f"lb: retries so far {lb.retries}")

    asyncio.create_task(report())
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)
