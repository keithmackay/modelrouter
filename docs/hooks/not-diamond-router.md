# Using Not Diamond for ML-Driven Auto-Routing via a Pipeline Hook

Not Diamond provides a client-side routing API that recommends which LLM to use for a given prompt. Integrate it as a `request.pre` pipeline hook — modelrouter pipes the request JSON to your script before forwarding to the upstream, and your script can override the `model` field.

## Prerequisites

```bash
pip install notdiamond
export NOTDIAMOND_API_KEY="your-key"
```

## Hook script

Create `/usr/local/bin/mr-notdiamond-hook.py`:

```python
#!/usr/bin/env python3
"""
modelrouter request.pre hook — overrides model with Not Diamond's recommendation.
Reads request JSON from stdin, writes (possibly mutated) JSON to stdout.
"""
import json
import os
import sys

# Map Not Diamond model IDs → modelrouter provider/model strings
MODEL_MAP = {
    "openai/gpt-4o":             "openai/gpt-4o",
    "openai/gpt-4o-mini":        "openai/gpt-4o-mini",
    "anthropic/claude-opus-4-5": "anthropic/claude-opus-4-5",
    "anthropic/claude-haiku-4-5":"anthropic/claude-haiku-4-5",
    "google/gemini-1.5-pro":     "google/gemini-1.5-pro",
    "deepseek/deepseek-chat":    "deepseek/deepseek-chat",
}

def main():
    body = json.load(sys.stdin)
    messages = body.get("messages", [])
    if not messages:
        json.dump(body, sys.stdout)
        return

    try:
        from notdiamond import NotDiamond
        client = NotDiamond(api_key=os.environ["NOTDIAMOND_API_KEY"])

        nd_messages = [
            {"role": m["role"], "content": m.get("content", "")}
            for m in messages
            if isinstance(m.get("content"), str)
        ]

        _result, _session_id, provider = client.chat.completions.model_select(
            messages=nd_messages,
            model=list(MODEL_MAP.keys()),
        )

        recommended = MODEL_MAP.get(str(provider))
        if recommended:
            body["model"] = recommended

    except Exception as e:
        print(f"not-diamond hook error: {e}", file=sys.stderr)
        # fail-open: pass through original request

    json.dump(body, sys.stdout)

if __name__ == "__main__":
    main()
```

```bash
chmod +x /usr/local/bin/mr-notdiamond-hook.py
```

## Config

Add this to your `config.toml`:

```toml
[[hooks.pipeline]]
name         = "not-diamond-router"
event        = "request.pre"
exec         = "/usr/local/bin/mr-notdiamond-hook.py"
capabilities = ["mutate_request"]
timeout_secs = 3
fail_open    = true
```

## Grant hook capability

After the hook is deployed, grant users permission to use it by inserting into the `hook_permissions` table:

```sql
INSERT INTO hook_permissions (user_id, hook_name, capability)
VALUES (<user_id>, 'not-diamond-router', 'mutate_request');
```

Or for all users, use a policy rule in `config.toml`:

```toml
[[policy_rules]]
match        = ["all"]
hook_capabilities = ["not-diamond-router:mutate_request"]
```

## How it works

1. A request arrives at modelrouter's `/v1/chat/completions` endpoint with an original `model` field.
2. modelrouter identifies the `not-diamond-router` hook (after auth and budget checks) and serializes the request body to JSON.
3. The hook script receives the JSON on stdin, extracts the `messages`, and calls Not Diamond's `model_select()` API.
4. Not Diamond returns a recommended provider/model based on prompt content.
5. The script maps the recommendation back to a modelrouter provider string (e.g. `anthropic/claude-opus-4-5`) and overwrites the `model` field.
6. The mutated JSON is written to stdout.
7. modelrouter resumes routing using the Not Diamond-recommended model.

If the Not Diamond API is unavailable, slow, or returns an error:
- The hook returns a 500 error and modelrouter responds with `500 Hook failed` unless `fail_open = true`
- If `fail_open = true`, modelrouter passes through the original request unchanged and logs the hook error

## Cost

Not Diamond charges per routing call (not per token) — typically far less than the token savings from optimal model selection. At high request volumes, consider caching routing decisions by prompt hash to reduce Not Diamond API calls.

## Performance

Not Diamond's `model_select()` typically takes 100–200ms. For low-latency applications, either:
- Cache routing decisions at the client (use the same model for similar prompts)
- Use `:fastest` routing shortcut instead of ML-driven selection
- Increase the hook `timeout_secs` if your provider latencies are high
