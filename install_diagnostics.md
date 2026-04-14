# modelrouter — Installation Diagnostics

Captured from initial deployment session on 2026-04-13.

---

## Issue 1: Circuit Breaker Tripped After Initial Failures

**Symptom:** All requests return `502 Bad Gateway` immediately (~15–20ms), even after correcting other configuration errors.

**Log signature:**
```
WARN circuit breaker open, skipping provider provider="anthropic"
ERROR response failed classification=Status code: 502 Bad Gateway latency=19ms
```

**Cause:** modelrouter uses a per-provider circuit breaker. Once a provider fails enough times, the breaker opens and all subsequent requests are rejected without attempting the API — even if the underlying problem has been fixed.

**Correction:** Restart the container to reset in-memory circuit breaker state:
```bash
docker-compose restart modelrouter
```

**Condition required for normal operation:** The circuit breaker must be in a closed (healthy) state. If you fix a configuration error, always restart the container before retesting.

---

## Issue 2: Zscaler SSL Inspection — TLS Certificate Verification Failure

**Symptom:** Requests fail with `"Failed to send request to Anthropic"` in ~150–280ms. TCP connects successfully but TLS handshake fails. The Anthropic API works fine from the host (`curl` directly), but fails from inside the container.

**Log signature:**
```
DEBUG reqwest::connect: starting new connection: https://api.anthropic.com/
DEBUG hyper_util: connecting to 160.79.104.10:443
DEBUG hyper_util: connected to 160.79.104.10:443
WARN Provider call failed error=Failed to send request to Anthropic
ERROR response failed Status code: 502 Bad Gateway latency=178ms
```

**Cause:** Zscaler is installed and running as a corporate SSL inspection proxy (`/Applications/Zscaler/Zscaler.app`). It intercepts all outbound HTTPS via a PAC file (`http://127.0.0.1:9000/localproxy-*.pac`) and re-signs TLS certificates with the Zscaler Root CA. macOS trusts this CA (it is installed in `/Library/Keychains/System.keychain`), but Docker containers use a separate Linux certificate store and do not trust it by default.

**Confirmation test:**
```bash
# This fails (cert error, exit code 60):
docker run --rm alpine sh -c "apk add -q curl && curl -s https://api.anthropic.com/"

# This succeeds (cert verification skipped):
docker run --rm alpine sh -c "apk add -q curl && curl -sk https://api.anthropic.com/"
```

**Correction:**

1. Extract the Zscaler Root CA from the macOS System keychain:
   ```bash
   security find-certificate -c "Zscaler Root CA" -p /Library/Keychains/System.keychain \
     > certs/zscaler-root-ca.pem
   ```

2. Inject it into the Docker image (already done in `Dockerfile`):
   ```dockerfile
   COPY --chown=root:root certs/zscaler-root-ca.pem /usr/local/share/ca-certificates/zscaler-root-ca.crt
   RUN update-ca-certificates
   ```

3. Rebuild the image (with otel feature):
   ```bash
   docker build --build-arg FEATURES="otel" -t modelrouter:otel -t modelrouter:latest .
   ```

**Condition required for normal operation:**
- `certs/zscaler-root-ca.pem` must be present in the repo root before building
- The image must be rebuilt any time the Zscaler Root CA cert is rotated by your organization
- If Zscaler is ever removed or the machine moves to a non-Zscaler network, this cert is harmless but no longer needed

---

## Issue 3: OTEL Collector Unreachable — DNS Resolution Failure

**Symptom:** Continuous error log spam every ~30 seconds, even when requests succeed:
```
ERROR opentelemetry_sdk: name="BatchSpanProcessor.Flush.ExportError"
  reason="ExportFailed(Status { code: Unavailable, message: \"dns error\" })"
```

**Cause:** `config/config.toml` references `endpoint = "http://otel-collector:4317"`. This hostname was a service defined in a previous version of `docker-compose.yml` (`modelrouter-otel-collector-1`). That service was removed from the compose file, leaving the config pointing at a hostname that no longer resolves inside Docker.

**Correction:** Update `config/config.toml` to point to the actual OTLP collector. In this environment, Arize Phoenix serves as the OTLP collector:

1. Identify the Phoenix container name:
   ```bash
   docker ps --format "table {{.Names}}\t{{.Image}}" | grep phoenix
   ```

2. Connect Phoenix to the modelrouter Docker network (only needed if Phoenix was started outside of modelrouter's compose file):
   ```bash
   docker network connect modelrouter_default <phoenix-container-name>
   ```

3. Update `config/config.toml`:
   ```toml
   [telemetry]
   enabled = true
   endpoint = "http://<phoenix-container-name>:4317"
   ```

4. The config hot-reloads every 30 seconds — no container restart needed.

**Condition required for normal operation:**
- The OTLP collector container must be running and reachable on port 4317 from within the `modelrouter_default` Docker network
- The container name in `config/config.toml` must match the actual running container name
- If Phoenix is managed by its own `docker-compose.yml`, consider adding a shared Docker network or joining modelrouter to Phoenix's network at startup

---

## Issue 4 (Bug): Router Returns Prefixed Model Name to Anthropic — 404 → Circuit Breaker

**Symptom:** Requests to `/v1/messages` using bare model names like `claude-sonnet-4-6` (no provider prefix) receive a 502. Logs show no "circuit breaker open" warning on the first few requests, then switch to circuit breaker rejections. The actual Anthropic error (before circuit breaker trips) is:
```
provider_error: Anthropic API error 404: {"error":{"type":"not_found_error","message":"model: anthropic/claude-sonnet-4-6"}}
```

**Cause:** Bug in `src/router/engine.rs`. When a model name has no provider prefix and no alias match, `resolve()` falls back to `(default_provider, default_model)`. But `default_model` in config is `"anthropic/claude-sonnet-4-6"` — the full prefixed form. The second element of the tuple (used as the bare model name sent to Anthropic) was being returned as `"anthropic/claude-sonnet-4-6"` instead of `"claude-sonnet-4-6"`.

**Correction:** Fixed in `src/router/engine.rs` — the fallback path now strips the provider prefix from `default_model`:
```rust
// Before (broken):
(
    self.settings.routing.default_provider.clone(),
    self.settings.routing.default_model.clone(), // was "anthropic/claude-sonnet-4-6"
)

// After (fixed):
let default = &self.settings.routing.default_model;
if let Some(pos) = default.find('/') {
    (default[..pos].to_string(), default[pos + 1..].to_string())
} else {
    (self.settings.routing.default_provider.clone(), default.clone())
}
```

**Why it's hard to spot:** The `/v1/chat/completions` endpoint uses the AnthropicAdapter which is only reached with explicitly prefixed models (e.g. `anthropic/claude-haiku-4-5-20251001`), so the bug is only triggered on the `/v1/messages` endpoint with bare model names — exactly what Claude Code sends. The Anthropic 404 trips the circuit breaker, so all subsequent requests fail instantly, masking the underlying model-name error.

**Condition required for normal operation:** Rebuild the image after this fix:
```bash
docker build --build-arg FEATURES="otel" -t modelrouter:otel -t modelrouter:latest .
docker-compose up -d --no-build
```

---

## Issue 5: OTEL Metrics Endpoint Not Implemented in Phoenix

**Symptom:** After connecting to Phoenix successfully, a different error appears:
```
ERROR opentelemetry_sdk: name="PeriodicReader.ExportFailed"
  reason="Metrics exporter otlp failed with the grpc server returns error
  (Operation is not implemented or not supported): Method not found!"
```

**Cause:** modelrouter exports both traces and metrics via OTLP gRPC. Arize Phoenix implements the OTLP traces service (`opentelemetry.proto.collector.trace.v1.TraceService`) but does not implement the metrics service (`opentelemetry.proto.collector.metrics.v1.MetricsService`). The gRPC server returns "Method not found" for metric export calls.

**Impact:** Traces and spans are received and visible in the Phoenix UI. Metrics (token counts, latency histograms, etc.) are not exported. This is a Phoenix limitation, not a modelrouter bug.

**Correction options:**
- **Accept it** — traces still flow; the error is cosmetic noise
- **Add a full OTLP collector** (e.g. OpenTelemetry Collector, Grafana Alloy) in front of Phoenix to receive both traces and metrics and fan them out
- **Disable metrics export** if a config option exists in a future modelrouter release

---

## Required Conditions Summary

For modelrouter to operate correctly in this environment:

| Condition | Details |
|-----------|---------|
| Image built from patched source (router bug fix) | `src/router/engine.rs` fix must be present; do not use unpatched `modelrouter:otel` image |
| Docker image built with Zscaler CA cert | `certs/zscaler-root-ca.pem` must exist; rebuild image when cert rotates |
| OTLP collector reachable at configured endpoint | Container must be on `modelrouter_default` network; hostname in config must match |
| Anthropic API key valid in `config/config.toml` | `[providers.anthropic] api_key = "sk-ant-..."` |
| No stale circuit breaker state | Restart container after fixing configuration errors |
| `config/config.toml` mounted at `/config/config.toml` | Via volume mount in `docker-compose.yml` |
| Database directory writable at `/data` | `./data:/data` volume must exist and be writable |

---

## Quick Verification Commands

```bash
# Test the proxy is accepting requests (expect 200)
curl -s -w "\n%{http_code}" -X POST http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer <your-mr-key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"anthropic/claude-haiku-4-5-20251001","messages":[{"role":"user","content":"hi"}],"max_tokens":5}'

# Check for errors in logs
docker logs modelrouter-modelrouter-1 2>&1 | grep -E "ERROR|WARN" | grep -v "hot-reloaded" | tail -20

# List provisioned API keys
docker-compose exec modelrouter ./modelrouter key list

# List users
docker-compose exec modelrouter ./modelrouter user list

# Clean up stale triggers (GAS add-on — unrelated but noted for completeness)
# Run cleanUpTriggers() from the GAS editor if forwarding jobs get stuck
```
