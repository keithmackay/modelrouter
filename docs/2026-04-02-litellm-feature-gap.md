# Feature Gap Analysis: modelrouter vs. LiteLLM Proxy

_Generated: 2026-04-02_

This report lists features present in **LiteLLM Proxy** that are absent or incomplete in **modelrouter v0.1.0**, with a value assessment for each. Features are grouped by theme and prioritised within each group.

> **Note on LiteLLM source:** LiteLLM was not available at `../litellm` during this analysis; the comparison is based on LiteLLM's public documentation and known stable feature set (as of early 2026).

---

## 1. Provider Coverage

### 1.1 Azure OpenAI
modelrouter has no Azure OpenAI adapter. LiteLLM treats Azure as a first-class provider with native support for managed identity, regional endpoints, and API version negotiation.

**Value:** Unblocks enterprise teams whose Anthropic/OpenAI consumption must run through Azure-hosted endpoints for compliance, data-residency, or billing reasons. Required for any Microsoft EA customer.

### 1.2 AWS Bedrock
No Bedrock adapter. LiteLLM supports Claude on Bedrock, Titan, Command, Llama via Bedrock, and Stable Diffusion.

**Value:** Teams running in AWS who want spend consolidated through Bedrock (for reserved capacity, private endpoints, or IAM-based access control) cannot use modelrouter today.

### 1.3 Cohere, Mistral, xAI/Grok, Groq, Perplexity, Together AI, OpenRouter, and 40+ others
modelrouter has 4 providers; LiteLLM has 60+.

**Value:** Each missing provider is a hard blocker for teams that use that model. Groq in particular is heavily used for high-throughput, low-latency inference at prices lower than OpenAI's hosted endpoints. OpenRouter provides access to dozens of models through a single API key — useful for teams that want model diversity without credential proliferation.

### 1.4 Vertex AI (Google)
modelrouter supports Gemini via Google's OpenAI-compatible endpoint but not via Vertex AI directly.

**Value:** Vertex AI is the required path for Google Cloud customers with enterprise agreements, VPC Service Controls, or data-residency requirements. The OpenAI-compat shim doesn't cover all Vertex-specific models.

---

## 2. Request Modalities

### 2.1 Embeddings (`POST /v1/embeddings`)
modelrouter only proxies chat completions. LiteLLM proxies embeddings for OpenAI, Cohere, Azure, Bedrock, Vertex AI, and Hugging Face.

**Value:** RAG pipelines, semantic search, and clustering workflows all require embeddings. Without this, teams need a second proxy or direct provider access for embedding workloads, splitting budget visibility.

### 2.2 Image Generation (`POST /v1/images/generations`)
Not implemented. LiteLLM supports DALL-E 2/3, Stable Diffusion via Bedrock, and Replicate.

**Value:** Product teams using image generation alongside text need a single gateway for unified cost tracking and access control.

### 2.3 Audio Transcription and Speech (`POST /v1/audio/transcriptions`, `/v1/audio/speech`)
Not implemented. LiteLLM supports Whisper (transcription) and OpenAI TTS.

**Value:** Voice-driven products and call-centre automation require these endpoints. Without them, audio workflows bypass the proxy entirely, creating a budget visibility gap.

### 2.4 Legacy Completions (`POST /v1/completions`)
Not implemented. LiteLLM supports the non-chat completions endpoint.

**Value:** Older tooling and some fine-tuned model deployments still use `/v1/completions`. Not critical for new projects, but a barrier to adoption for teams migrating legacy systems.

---

## 3. Routing and Load Balancing

### 3.1 Load Balancing Across Providers
modelrouter has fallback chains (config exists, retry logic partially implemented). LiteLLM provides round-robin, least-busy, latency-based, cost-optimised, and weighted routing across a pool of equivalent models/providers.

**Value:** Production deployments need redundancy and capacity spreading. Round-robin across multiple OpenAI API keys prevents rate-limit exhaustion. Latency-based routing reduces p99 for interactive users. Cost-optimised routing automatically shifts traffic to the cheapest available option within acceptable latency.

### 3.2 Shadow / Canary Traffic
Not implemented. LiteLLM can mirror a fraction of live traffic to an alternate provider without affecting the user-facing response.

**Value:** Enables safe evaluation of new models or providers using real traffic without user risk. Essential for model migration projects.

### 3.3 Request Queuing Under Rate Limits
When a user hits a rate limit, modelrouter returns 429 immediately. LiteLLM queues requests and retries transparently.

**Value:** For batch workloads and background agents, transparent queuing yields much better throughput than forcing callers to implement retry logic. Reduces client complexity significantly.

### 3.4 Circuit Breaker
Not implemented. LiteLLM stops sending traffic to a failing provider until it recovers.

**Value:** Prevents cascading failures when a provider is degraded. Without a circuit breaker, a slow provider increases latency for all users via timeout wait rather than fast-failing to a fallback.

---

## 4. Caching

### 4.1 Semantic and Exact Response Caching
Not implemented. LiteLLM caches responses by exact prompt match and (optionally) by semantic similarity, with Redis or in-memory backends. Cache hits are free — the provider is never called.

**Value:** For repetitive workloads (FAQ bots, fixed-prompt pipelines, CI test suites), caching eliminates 30–80% of provider spend. This is one of the highest-ROI features for cost reduction. Cache hit rates are reported in the dashboard.

---

## 5. Budget and Spend Controls

### 5.1 Per-Key Budgets
modelrouter ties budgets to users. LiteLLM has a separate API key entity — multiple keys per user, each with its own budget — enabling per-application or per-environment spend controls independent of the user identity.

**Value:** A single user (e.g., a developer) running three different applications can have separate budgets per app without creating three separate user accounts. Also enables CI/CD pipelines to have their own spend envelope.

### 5.2 Enforced Token-Volume Limits
modelrouter has a `limit_tokens` column in `budget_rules` but the policy engine doesn't enforce it. LiteLLM enforces token-per-minute and token-per-day limits.

**Value:** Dollar limits lag behind token consumption by up to a request cycle. Token limits give tighter, more predictable control for high-volume workloads where you care about throughput rather than cost.

### 5.3 Organisation / Team Hierarchy
modelrouter has users and groups (one level). LiteLLM has a three-level hierarchy: organisation → team → user → key.

**Value:** Enables enterprise allocation — an org-level budget can be subdivided across teams, each of which subdivides further to individual developers. Without this, large organisations must either use one flat budget or manage per-user limits manually.

### 5.4 Spend Reset API
Not implemented. LiteLLM has `POST /admin/spend/reset` to zero out counters.

**Value:** Useful for end-of-billing-cycle resets, testing, and correcting data entry errors without touching the database directly.

### 5.5 Custom Pricing Tables
modelrouter has hardcoded pricing in `router/cost.rs`. LiteLLM accepts custom price definitions per model in the config.

**Value:** Operator-negotiated pricing (enterprise agreements with providers) will differ from public list rates. Hardcoded pricing produces incorrect cost attribution for anyone on a custom rate. Also affects internally hosted models where the cost is infrastructure spend, not API spend.

---

## 6. Observability Integrations

### 6.1 Prometheus Metrics Export
Not implemented. LiteLLM exposes a `/metrics` endpoint in Prometheus format.

**Value:** Teams already running Prometheus + Grafana can add modelrouter dashboards without any additional infrastructure. OTLP is more powerful but has a higher adoption barrier — many teams have Prometheus but not an OTLP collector.

### 6.2 LLM-Specific Observability Platforms
LiteLLM has native integrations with LangSmith, LangFuse, and Helicone — platforms purpose-built for LLM prompt/response observability, dataset collection, and evaluation.

**Value:** These platforms offer prompt-level replay, regression testing, and evaluation workflows that general-purpose OTel backends (including Phoenix) don't provide. Teams building LLM applications use them to catch regressions when switching models or prompts.

### 6.3 External Log Destinations (S3, DynamoDB, Datadog, New Relic)
modelrouter logs to its local database only. LiteLLM can forward logs to S3, DynamoDB, Datadog, and New Relic.

**Value:** S3 + Athena is a standard pattern for cheap long-term retention and ad-hoc querying of large request logs. Datadog and New Relic are the existing observability platforms in many organisations — integration eliminates the need to run a separate query interface.

### 6.4 Sentry Error Tracking
Not implemented.

**Value:** Captures and groups provider errors, timeouts, and unexpected failures with stack traces and occurrence counts. Reduces alert fatigue compared to raw log scanning.

---

## 7. Rate Limiting

### 7.1 IP-Based Rate Limiting
modelrouter rate-limits by user only. LiteLLM supports rate limits keyed by source IP address.

**Value:** Defends against unauthenticated/pre-auth abuse and DDoS amplification. Without IP-level limiting, a single unauthenticated client can flood the server's auth path.

### 7.2 Concurrent Request Limits
Not implemented. LiteLLM can cap the number of in-flight requests per key/user.

**Value:** Prevents a single user from saturating downstream provider connections, which would degrade latency for everyone else even if their dollar budget hasn't been hit.

---

## 8. Security and Identity

### 8.1 SSO / SAML Integration
Not implemented. LiteLLM supports enterprise SSO for the admin dashboard.

**Value:** Required for enterprise deployments where IT mandates centralised identity management. Without SSO, admins maintain a separate credential set, creating a security audit gap and onboarding/offboarding friction.

---

## 9. Deployment and Operations

### 9.1 Kubernetes / Helm Charts
modelrouter ships a Dockerfile. LiteLLM provides Helm charts with HPA, PVC, liveness/readiness probes, and values files for common configurations.

**Value:** Teams operating in Kubernetes need Helm to manage deployments reproducibly. Without a chart, each team writes their own K8s YAML, creating drift and maintenance burden. HPA support is particularly important for handling traffic spikes without manual intervention.

### 9.2 Config Hot-Reload
Not implemented. modelrouter requires a restart to pick up config changes. LiteLLM can reload parts of the config at runtime.

**Value:** For operations teams managing a shared gateway, restarting to add a new user or adjust a budget limit causes brief downtime and disrupts in-flight streaming requests.

### 9.3 Auto-Generated API Documentation (`/docs`)
Not implemented. LiteLLM (FastAPI-based) exposes Swagger UI at `/docs` automatically.

**Value:** Reduces integration time for new teams and makes the proxy's interface self-documenting. Swagger also enables testing directly from the browser without `curl`.

---

## 10. Additional Modalities and Data Enrichment

### 10.1 Multi-Modal Inputs (Images in Prompts)
Not implemented. LiteLLM normalises image inputs across providers that support vision (GPT-4o, Claude 3, Gemini 1.5).

**Value:** Vision workflows are increasingly common in document processing, screen-reading agents, and product analysis. Without this, vision requests bypass the proxy.

---

## Summary Prioritisation

The table below scores each gap by **impact** (how broadly it affects adoption) and **implementation effort** (rough estimate relative to the existing codebase):

| # | Feature | Impact | Effort | Notes |
|---|---------|--------|--------|-------|
| 1 | Enforced token limits | High | Low | Schema exists; policy engine change only |
| 2 | Custom pricing tables | High | Low | Replace hardcoded map with config-driven table |
| 3 | Fallback chain / retry completion | High | Low | Config already exists; retry loop needed |
| 4 | Prometheus metrics endpoint | High | Low | Wrapper around existing OTel metrics |
| 5 | Semantic response caching | High | Medium | Redis integration + embedding similarity check |
| 6 | Embeddings endpoint | High | Medium | New route + provider adapters for embed API |
| 7 | Per-key budgets | Medium | Medium | New key entity separate from user |
| 8 | Azure OpenAI adapter | High | Medium | New provider adapter following existing pattern |
| 9 | AWS Bedrock adapter | High | Medium | New provider adapter; sigv4 auth required |
| 10 | Load balancing (round-robin, weighted) | High | Medium | Pool model + request dispatcher |
| 11 | Config hot-reload | Medium | Medium | Watch config file; partial state update |
| 12 | Spend reset API | Low | Low | Single SQL update + admin endpoint |
| 13 | Organisation / team hierarchy | Medium | High | Schema change + additional policy layer |
| 14 | IP-based rate limiting | Medium | Low | Add IP key to rate limit bucket lookup |
| 15 | Groq / Mistral / OpenRouter adapters | Medium | Low | OpenAI-compat adapter reuse for most |
| 16 | LangFuse / LangSmith integration | Medium | Medium | Callback-style hook or OTel exporter shim |
| 17 | Kubernetes / Helm charts | Medium | Medium | Ops work; no code changes to modelrouter itself |
| 18 | Concurrent request limits | Medium | Low | Semaphore per user in AppState |
| 19 | Circuit breaker | Medium | Medium | Provider-level failure state + backoff |
| 20 | SSO / SAML | Low | High | Third-party IdP integration |
| 21 | Request queuing | Low | Medium | Async queue per user |
| 22 | Image generation endpoint | Low | Medium | New route + DALL-E / Bedrock adapters |
| 23 | Shadow traffic routing | Low | High | Parallel request dispatch; response discarding |
| 24 | S3 / DynamoDB log export | Low | Low | Async write hook on existing log path |
| 25 | Audio (Whisper / TTS) | Low | Medium | New routes + provider adapters |

---

_End of report_
