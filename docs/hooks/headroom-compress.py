#!/usr/bin/env python3
"""
request.pre hook: compress messages via a local headroom proxy (CCR disabled).

Reads the modelrouter request body from stdin, compresses the messages array
through headroom's /v1/compress endpoint, writes the mutated body to stdout.

Environment variables:
  HEADROOM_URL   Base URL of the headroom proxy. Default: http://127.0.0.1:8787
  HEADROOM_TIMEOUT_SECS  HTTP timeout. Default: 4 (hook timeout_secs should be 5)

Install headroom (proxy extras required):
  pip install "headroom-ai[proxy]"

Start headroom with CCR disabled:
  HEADROOM_NO_CCR_INJECT_TOOL=1 headroom proxy --port 8787

Register this hook in config.toml — see README-HOOKS.md for full instructions.
"""

import json
import os
import sys
import urllib.request
import urllib.error

HEADROOM_URL = os.environ.get("HEADROOM_URL", "http://127.0.0.1:8787").rstrip("/")
HEADROOM_TIMEOUT = int(os.environ.get("HEADROOM_TIMEOUT_SECS", "4"))


def compress(body: dict) -> dict:
    messages = body.get("messages")
    if not messages:
        return body

    model = body.get("model", "gpt-4o")

    payload = json.dumps({
        "messages": messages,
        "model": model,
    }).encode()

    req = urllib.request.Request(
        f"{HEADROOM_URL}/v1/compress",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    try:
        with urllib.request.urlopen(req, timeout=HEADROOM_TIMEOUT) as resp:
            result = json.loads(resp.read())
    except urllib.error.URLError as e:
        # Headroom unreachable — fail_open means we return original body unchanged.
        # The error is written to stderr so modelrouter logs it.
        print(f"headroom-compress: unreachable ({e})", file=sys.stderr)
        return body
    except Exception as e:
        print(f"headroom-compress: unexpected error ({e})", file=sys.stderr)
        return body

    compressed_messages = result.get("messages")
    if not compressed_messages:
        return body

    tokens_before = result.get("tokens_before", "?")
    tokens_after = result.get("tokens_after", "?")
    ratio = result.get("compression_ratio")
    ratio_str = f" ({ratio:.0%} kept)" if ratio is not None else ""
    print(
        f"headroom-compress: {tokens_before} → {tokens_after} tokens{ratio_str}",
        file=sys.stderr,
    )

    body = dict(body)
    body["messages"] = compressed_messages
    return body


if __name__ == "__main__":
    try:
        body = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        print(f"headroom-compress: invalid JSON on stdin ({e})", file=sys.stderr)
        sys.exit(1)

    result = compress(body)
    json.dump(result, sys.stdout)
