# Feature Gap Analysis: modelrouter vs. LiteLLM Proxy

_Generated: 2026-04-02 — updated after direct code review of `/Users/Keith.MacKay/Projects/litellm`_

This report lists features present in **LiteLLM Proxy** that are absent or incomplete in **modelrouter v0.1.0**, with a value assessment for each. The first version of this document was based on public documentation; this version was produced from a full read of the LiteLLM source tree (`litellm/proxy/`, `litellm/caching/`, `enterprise/`, `deploy/`) and contains several additions and corrections.

---

## Notable additions vs. the first draft

The following feature areas were absent from the documentation-based analysis and were found by reading the code:

- **Native Anthropic Messages API endpoint** (`POST /v1/messages`) — direct passthrough, crucial for Claude Code
- **MCP (Model Context Protocol)** — full MCP server registry, tool calling, semantic tool filtering
- **WebSocket / Realtime API** (`/v1/realtime`, `/vertex_ai/live`) — voice and real-time text
- **Vector stores and RAG** — file uploads, vector store management, retrieval endpoints
- **Batch API** (`/v1/batches`) — OpenAI Batch API compatibility with cost tracking
- **OpenAI Responses API** (`/v1/responses`) — structured outputs passthrough
- **Fine-tuning endpoint** (`/v1/fine_tuning/jobs`)
- **Cascading policy engine** — declarative rules that inherit org → team → project → key
- **SCIM** — automated enterprise user provisioning
- **Billing integrations** (Stripe, Lago)
- **Prompt management** — versioned stored prompt templates
- **Agent/agentic workflow endpoints**
- **ML-based auto-router and complexity router**
- **Semantic MCP tool filtering** (embedding-based context window compression)
- **Cold storage for spend logs** (S3 archival)
- **Per-tag budgets**
- **Session-based rate limits** for agent loops
- **Key regeneration** (new token, preserved metadata)
- **Cost platform integrations** (Cloudzero, Vantage)

---

## 1. API Endpoints and Request Modalities

### 1.1 Native Anthropic Messages API (`POST /v1/messages`)
LiteLLM implements the Anthropic Messages API format natively and passes it through to Anthropic or Bedrock. modelrouter only exposes `POST /v1/chat/completions` (OpenAI format).

**Value:** This is the single most impactful gap for a Claude Code deployment. Claude Code sends requests in Anthropic Messages API format. Without a `/v1/messages` endpoint on modelrouter, Claude Code cannot route through modelrouter at all without an additional format-translation layer. Adding this endpoint would eliminate the ANTHROPIC_BASE_URL compatibility caveat in the README walkthrough.

### 1.2 Embeddings (`POST /v1/embeddings`)
Not implemented. LiteLLM proxies embeddings for OpenAI, Cohere, Azure, Bedrock Titan, Vertex AI, Hugging Face, Jina, and Voyage AI.

**Value:** RAG pipelines, semantic search, and classification workflows all require embeddings. Without this, teams need a second gateway or direct provider access, splitting budget visibility across systems.

### 1.3 Image Generation (`POST /v1/images/generations`)
Not implemented. LiteLLM supports DALL-E 2/3, Stability AI, Black Forest Labs, Recraft, and Replicate.

**Value:** Product teams using image generation alongside text need a single gateway for unified cost tracking and access control.

### 1.4 Audio Transcription and Speech (`POST /v1/audio/*`)
Not implemented. LiteLLM supports OpenAI Whisper, Azure Speech, Eleven Labs, and Google TTS.

**Value:** Voice-driven products, call-centre automation, and transcription pipelines bypass the proxy entirely today, creating a budget and access control gap.

### 1.5 Reranking (`POST /v1/rerank`)
Not implemented. LiteLLM supports Jina, Cohere, and Mixedbread AI reranking models.

**Value:** Reranking is the second step in most two-stage RAG pipelines. Without it, the retrieval half of RAG bypasses the proxy.

### 1.6 Batch API (`POST /v1/batches`)
Not implemented. LiteLLM implements full OpenAI Batch API compatibility including status polling and result retrieval, with a background job that calculates costs after batch completion.

**Value:** Batch processing is 50% cheaper than real-time on OpenAI. Teams running large eval suites, nightly data pipelines, or document-processing workloads need batch support.

### 1.7 OpenAI Responses API (`POST /v1/responses`)
Not implemented. LiteLLM passes through the OpenAI Responses API, which is the new structured-output primitive replacing function calling.

**Value:** The Responses API is OpenAI's forward-looking interface for tool use and structured output. SDKs are moving to it; not supporting it creates a compatibility cliff.

### 1.8 Fine-Tuning (`POST /v1/fine_tuning/jobs`)
Not implemented.

**Value:** Organisations running fine-tuning jobs need the same budget tracking, audit trail, and access controls as inference. Without this, fine-tuning spend is invisible to modelrouter.

### 1.9 WebSocket / Realtime (`/v1/realtime`, `/vertex_ai/live`)
Not implemented. LiteLLM supports WebSocket-based realtime streaming for voice and real-time text (OpenAI Realtime API, Vertex AI live).

**Value:** Realtime voice agents are an increasingly mainstream workload. They require persistent WebSocket connections, not HTTP, so the existing proxy model doesn't cover them.

---

## 2. Provider Coverage

### 2.1 Azure OpenAI
No adapter. LiteLLM supports Azure OpenAI with managed identity, regional endpoints, and deployment-specific API versions.

**Value:** Unblocks enterprise teams whose LLM consumption must run through Azure-hosted endpoints for compliance, data-residency, or billing reasons. Required for any Microsoft EA customer.

### 2.2 AWS Bedrock
No adapter. LiteLLM supports all Bedrock models (Claude via Converse + Invoke APIs, Titan embeddings, Stable Diffusion, Amazon Nova) with SigV4 authentication.

**Value:** Teams running in AWS who route consumption through Bedrock for reserved capacity, private endpoints, or IAM-based access control cannot use modelrouter today.

### 2.3 Groq, Mistral, DeepSeek, xAI/Grok, Perplexity, Together AI, OpenRouter, and 90+ others
modelrouter has 4 providers; LiteLLM has 100+. For the majority of missing providers, LiteLLM reuses its OpenAI-compat adapter — the incremental effort per provider is low.

**Value:** Each missing provider is a hard blocker for teams using that model. Groq is particularly relevant — it offers OpenAI-compatible inference at substantially lower latency and cost than OpenAI's hosted endpoints. OpenRouter provides model diversity through a single API key, useful for teams that want redundancy without credential proliferation.

### 2.4 Vertex AI (Google Cloud)
modelrouter reaches Gemini via Google's OpenAI-compatible shim but not Vertex AI proper.

**Value:** Vertex AI is the required path for Google Cloud customers with VPC Service Controls or data-residency requirements. The shim doesn't cover all Vertex-native models or enterprise features.

---

## 3. Routing and Load Balancing

### 3.1 Actual Load Balancing
modelrouter has fallback chains in config but the retry loop is not yet implemented. LiteLLM provides: simple shuffle, lowest-latency routing, lowest-cost routing, least-busy, TPM/RPM-weighted distribution, tag-based routing, and a background ML-based auto-router.

**Value:** Production deployments need both redundancy and capacity spreading. Round-robin across multiple API keys for the same model prevents rate-limit exhaustion. Latency-based routing reduces p99 for interactive users. Cost-optimised routing automatically shifts traffic to the cheapest available option within an acceptable latency envelope.

### 3.2 ML Auto-Router (`router_strategy/auto_router/`)
LiteLLM includes a self-learning router that adapts deployment selection based on observed latency, cost, and success rates.

**Value:** Eliminates the need for manual routing weight tuning as provider performance shifts over time.

### 3.3 Complexity Router
Routes by estimated prompt complexity — simple prompts go to a cheaper/faster model, complex ones to a capable one.

**Value:** Direct cost reduction without changing client code. Teams that today pay for opus-class models on every request could automatically route simple queries to haiku-class models.

### 3.4 Shadow / Canary Traffic
Not implemented. LiteLLM can silently mirror a fraction of live traffic to an alternate provider without affecting the user-facing response.

**Value:** Enables safe evaluation of new models or providers against real production traffic. Essential for model migration projects.

### 3.5 Request Queuing Under Rate Limits
modelrouter returns 429 immediately. LiteLLM queues requests and retries transparently.

**Value:** For batch workloads and background agents, transparent queuing yields much better throughput than forcing callers to implement retry logic.

### 3.6 Circuit Breaker
Found in `router_strategy/` — LiteLLM stops routing to deployments that are failing until they recover, using a cooldown mechanism.

**Value:** Prevents cascading failures when a provider is degraded. Without a circuit breaker, a slow provider increases latency for all users via timeout wait rather than fast-failing to a fallback.

---

## 4. Caching

### 4.1 Semantic and Exact Response Caching
Not implemented. LiteLLM has a multi-layer cache: in-memory LRU → Redis → optional S3/GCS/Azure Blob. Semantic caching uses Qdrant for embedding-based similarity matching.

**Value:** For repetitive workloads (FAQ bots, CI test suites, fixed-prompt pipelines), caching eliminates 30–80% of provider spend. Semantic caching extends this to prompts that ask the same thing in different words. This is one of the highest-ROI features LiteLLM offers.

### 4.2 Cache Management Endpoints
`GET /cache/ping`, `POST /cache/delete`, `GET /cache/redis/info`, `POST /cache/flushall` — observable and operable.

**Value:** Operations teams can inspect cache health, purge stale entries after a prompt change, and monitor Redis client connections without touching the database.

---

## 5. Budget and Spend Controls

### 5.1 Four-Level Hierarchy: Org → Team → Project → Key
modelrouter has two levels: user and group. LiteLLM has four: organisation, team, project, and individual API key — each with independent spend limits, model access lists, and rate limits.

**Value:** Enables enterprise allocation — an org-level budget subdivides across teams, each of which subdivides to individual developers. Without this, large organisations must either use a flat budget or manage per-user limits manually.

### 5.2 Per-Key Budgets
modelrouter ties budgets to users. LiteLLM has a separate API key entity — multiple keys per user, each with its own budget.

**Value:** A single developer running three applications (dev, staging, CI) can have separate spend envelopes per application without creating three user accounts.

### 5.3 Per-Tag Budgets
LiteLLM supports budgets keyed by arbitrary tag strings applied to keys.

**Value:** Enables cross-cutting budget groupings that don't follow the org/team/user hierarchy — for example, "project-X" might span members of multiple teams.

### 5.4 Per-Model Budgets (`model_max_budget`)
LiteLLM can cap spend on a specific model within a budget period, independent of the overall key budget.

**Value:** Prevents accidental use of expensive models (e.g., claude-opus) from consuming a budget that should mostly be spent on cheaper ones.

### 5.5 Enforced Token-Volume Limits (TPM/RPM)
modelrouter has `limit_tokens` in the schema but the policy engine doesn't enforce it. LiteLLM enforces tokens-per-minute and requests-per-minute with a sliding window in Redis, and also supports soft budgets (warn but don't reject).

**Value:** Dollar limits lag behind token consumption by up to a request cycle. Token limits give tighter, more predictable control for high-volume workloads. Soft budgets allow teams to monitor overage without hard-stopping production traffic.

### 5.6 Session-Based Rate Limits for Agent Loops
LiteLLM has `session_tpm_limit` and `session_rpm_limit` that apply per agent session rather than per user.

**Value:** Prevents a single runaway agent loop from consuming a user's entire budget in seconds, without affecting that user's interactive requests.

### 5.7 Custom Pricing Tables
modelrouter has hardcoded pricing in `router/cost.rs`. LiteLLM accepts per-deployment cost overrides in config.

**Value:** Operator-negotiated enterprise pricing differs from public rates. Also essential for internally hosted models where cost is infrastructure spend, not API spend.

### 5.8 Spend Reset API
`POST /admin/spend/reset` — zero out counters programmatically.

**Value:** Useful for end-of-billing-cycle resets, testing, and correcting data entry errors without touching the database directly.

### 5.9 Spend Alerts (Slack, Email)
LiteLLM has background jobs that send weekly/monthly spend reports to Slack channels and email addresses.

**Value:** Surfaces budget trends to managers who don't log into the admin dashboard. Catches runaway spend before end-of-month surprises.

### 5.10 Billing Platform Integrations (Stripe, Lago)
Found in enterprise: LiteLLM can push usage events to Stripe and Lago for downstream billing.

**Value:** Enables operators to monetise API access (charge end-users) or integrate with existing subscription billing systems without building a custom billing pipeline.

### 5.11 Cost Platform Integrations (Cloudzero, Vantage)
Found in `spend_tracking/`: LiteLLM syncs spend data to Cloudzero and Vantage for FinOps-style cloud cost management.

**Value:** Teams using Cloudzero or Vantage for cloud cost visibility can include LLM spend in the same dashboards as their AWS/GCP/Azure bills.

---

## 6. Rate Limiting

### 6.1 IP-Based Rate Limiting
modelrouter rate-limits by user only. LiteLLM supports rate limits keyed by source IP address.

**Value:** Defends against unauthenticated/pre-auth abuse and DDoS amplification.

### 6.2 Concurrent Request Limits (`max_parallel_requests`)
Not implemented. LiteLLM caps in-flight requests per key/user.

**Value:** Prevents a single user from saturating downstream provider connections and degrading latency for everyone else, even when their dollar budget hasn't been hit.

---

## 7. Observability and Logging

### 7.1 LLM-Specific Observability Platforms (LangFuse, LangSmith, Braintrust, Arize)
modelrouter has OTel OTLP only. LiteLLM has native integrations with LangFuse, LangSmith, Braintrust, and Arize — platforms purpose-built for prompt-level replay, dataset collection, and LLM evaluation workflows.

**Value:** These platforms provide regression testing when switching models or prompts, eval harnesses, and production dataset collection that general-purpose OTel backends don't offer.

### 7.2 Prometheus Metrics Export (`GET /metrics`)
Not implemented. LiteLLM exposes a `/metrics` endpoint in Prometheus format.

**Value:** Teams already running Prometheus + Grafana can monitor modelrouter without deploying an OTLP collector. Lower adoption barrier than OTLP for most engineering teams.

### 7.3 External Log Destinations (CloudWatch, S3, Datadog, New Relic, Splunk, GCP Logging, Azure Sentinel)
modelrouter logs to its local database only. LiteLLM can forward logs to a wide range of external destinations.

**Value:** S3 + Athena is the standard pattern for cheap long-term retention and ad-hoc querying of large request logs. Datadog and New Relic are the existing observability platforms in many organisations — native integration eliminates a separate query interface.

### 7.4 Cold Storage for Spend Logs (S3 Archival)
LiteLLM has a cold storage handler that archives old spend log rows to S3 to keep the database size manageable.

**Value:** Without archival, spend_logs grows unboundedly at high traffic volumes and degrades query performance.

### 7.5 Error Tracking (Sentry, Bugsnag)
Not implemented.

**Value:** Groups and counts provider errors, timeouts, and unexpected failures with stack traces. Reduces alert fatigue compared to raw log scanning.

---

## 8. Guardrails and Content Safety

### 8.1 Built-In Guardrail Framework
modelrouter has a configurable shell-hook system that can be used for guardrails, but provides no built-in safety logic. LiteLLM has a first-class guardrails subsystem with pre-call and post-call hooks, found in `litellm/proxy/guardrails/`.

**Value:** Teams that need prompt injection detection, PII masking, or content moderation must build everything from scratch on modelrouter. LiteLLM provides both the framework and a catalogue of integrations.

### 8.2 Guardrail Provider Integrations (40+)
Confirmed in code: Presidio (PII), Azure Content Safety, OpenAI Moderation, Lakera AI, Panw Prisma AIDR, Crowdstrike AIDR, IBM Guardrails, AWS Bedrock Guardrails, and custom code execution guards.

**Value:** Enterprise security teams have existing vendor relationships in this space. Native integrations reduce integration work from weeks to a config line.

### 8.3 Tool Permission Control (`tool_permission.py`)
Per-key whitelist/blacklist of function-call tools. Not present in modelrouter.

**Value:** Prevents agents from invoking dangerous tools (file writes, code execution, external API calls) regardless of what the LLM requests.

---

## 9. MCP (Model Context Protocol)

### 9.1 MCP Server Registry
Not present in modelrouter at all. LiteLLM has `POST /v1/mcp/server`, `GET /v1/mcp/server`, `PUT`, `DELETE` — a full registry for MCP servers.

**Value:** MCP is rapidly becoming the standard interface for tool-enabled agents. As Claude Code and similar tools adopt MCP for tool calling, a proxy without MCP support becomes a bottleneck.

### 9.2 Semantic Tool Filtering
Found in `litellm_settings.mcp_semantic_tool_filter`: uses embeddings to select only the tools semantically relevant to the current user prompt before passing the tool list to the LLM.

**Value:** Reduces context window consumption and cost when an agent has access to a large tool catalogue. A 50-tool MCP server list sent verbatim to the LLM can cost thousands of tokens per request; filtering to the 5 most relevant tools reduces this by 90%.

### 9.3 MCP Curated Server Discovery
`GET /v1/mcp/discover` returns a curated list of popular MCP servers (Wikipedia, etc.) that admins can add to their registry.

**Value:** Reduces the operational cost of MCP adoption for teams that don't want to build custom tool servers.

---

## 10. Policy Engine

### 10.1 Declarative, Cascading Policy Rules
Found in `litellm/proxy/policy_engine/`. LiteLLM has a full policy engine with condition-based rules, policy resolution, and cascading inheritance from org → team → project → key. modelrouter's "policy engine" is a fixed sequence of budget/rate/model checks with no declarative layer.

**Value:** Complex governance requirements (e.g., "team-X can only use claude-haiku except for users with the `research` tag, who may use claude-opus up to $200/month") require a policy language. Implementing this in code for each new rule is not scalable.

---

## 11. Authentication and Identity

### 11.1 SSO / OAuth2 / OpenID Connect
Not implemented in modelrouter. LiteLLM supports OIDC via Okta, Azure AD, and Auth0 for the admin dashboard.

**Value:** Required for enterprise deployments where IT mandates centralised identity management. Without SSO, admins maintain a separate credential set, creating a security audit gap and onboarding/offboarding friction.

### 11.2 SCIM (Automated User Provisioning)
Not implemented. LiteLLM supports SCIM for automatic user and group synchronisation from an identity provider.

**Value:** At scale, manually creating and deactivating user accounts is an operational burden and a compliance risk. SCIM ensures deprovisioned employees lose proxy access automatically.

### 11.3 Key Expiration and Auto-Rotation
modelrouter supports key rotation on demand. LiteLLM also supports key TTL (automatic expiry) and scheduled auto-rotation via a background job (`process_rotations` runs hourly).

**Value:** Reduces the credential lifetime window. Short-lived keys are a security best practice that requires no manual process overhead.

### 11.4 Key Regeneration (New Token, Preserved Metadata)
`POST /key/regenerate` issues a new token while keeping all metadata, budgets, and permissions intact.

**Value:** Clean credential rotation workflow without needing to re-configure all the key's settings.

---

## 12. Deployment and Operations

### 12.1 Kubernetes / Helm Charts
modelrouter ships a Dockerfile only. LiteLLM provides Helm charts with HPA (horizontal pod autoscaling), PVC for persistent storage, liveness/readiness/startup probes, init containers for migrations, and values files for common configurations.

**Value:** Teams operating in Kubernetes need Helm to manage deployments reproducibly. Without a chart, each team writes their own K8s YAML, creating drift and maintenance burden.

### 12.2 Config Hot-Reload
Not implemented. modelrouter requires a restart to pick up config changes. LiteLLM runs a background `add_deployment` job every 10 seconds to sync new model deployments from the database without restarting.

**Value:** For a shared gateway, restarting to add a new user or model disrupts in-flight streaming requests and requires a deployment process.

### 12.3 Auto-Generated API Docs (`/docs`)
Not implemented. LiteLLM (FastAPI) exposes Swagger UI at `/docs` automatically.

**Value:** Reduces integration time for new teams and makes the proxy self-documenting.

---

## 13. Advanced Features Without Direct Equivalent

### 13.1 Prompt Management (Versioned Templates)
`POST /prompts/create`, `GET /prompts/{id}`, `PUT /prompts/{id}` — versioned stored prompt templates with variables, tagging, and access control.

**Value:** Centralises system prompt management. Teams can update a shared system prompt once and have all downstream applications pick it up, without each app managing its own prompt text.

### 13.2 Agent Endpoints
`POST /agents`, `GET /agents`, `POST /agents/{id}/execute` — store agent configurations and execute them with tool calling, session memory, and per-session budget limits.

**Value:** A managed agent execution layer with its own rate limiting prevents agent loops from running unbounded.

### 13.3 Vector Store and RAG
`POST /v1/vector_stores`, file upload, retrieval endpoints — manage vector stores for RAG pipelines through the proxy.

**Value:** Keeps RAG infrastructure under the same access control, budget tracking, and audit trail as inference.

### 13.4 Prompt Caching Support (Anthropic)
LiteLLM automatically translates requests to include Anthropic's `cache_control` parameter for Claude 3.5 Sonnet and Bedrock equivalents.

**Value:** Prompt caching reduces cost and latency for long system prompts that are repeated across many requests — a very common pattern in Claude Code sessions where the same tools/instructions are sent each turn.

---

## Summary Prioritisation

Items are scored by **impact** (how broadly the absence blocks adoption or saves money) and **effort** relative to the existing Rust codebase.

| # | Feature | Impact | Effort | Notes |
|---|---------|--------|--------|-------|
| 1 | `/v1/messages` Anthropic passthrough | Critical | Low | One route + auth reuse; unblocks Claude Code natively |
| 2 | Enforced token limits (TPM/RPM) | High | Low | Schema column exists; policy engine change only |
| 3 | Custom pricing tables | High | Low | Replace hardcoded map in `router/cost.rs` with config-driven table |
| 4 | Fallback chain / retry completion | High | Low | Config parses; retry loop not written |
| 5 | Prometheus `/metrics` endpoint | High | Low | Wrapper around existing OTel instruments |
| 6 | Complexity router (cheap model for simple prompts) | High | Low-Med | Model complexity heuristic + routing rule |
| 7 | Semantic response caching | High | Medium | Redis integration + hash strategy; skip semantic initially |
| 8 | Embeddings endpoint | High | Medium | New route + provider adapter for embed API |
| 9 | Per-key budgets | High | Medium | New key entity separate from user |
| 10 | Azure OpenAI adapter | High | Medium | New provider adapter; follows existing pattern |
| 11 | AWS Bedrock adapter | High | Medium | New provider adapter; SigV4 auth required |
| 12 | Load balancing (round-robin, weighted, latency) | High | Medium | Pool + request dispatcher |
| 13 | Groq / Mistral / DeepSeek / OpenRouter adapters | Medium | Low | OpenAI-compat adapter reuse for most |
| 14 | Circuit breaker | Medium | Low | Provider-level failure state + cooldown |
| 15 | IP-based rate limiting | Medium | Low | Additional key in rate limit bucket |
| 16 | Concurrent request limits | Medium | Low | Semaphore per user in AppState |
| 17 | Spend reset API | Medium | Low | Single SQL update + admin endpoint |
| 18 | Per-tag budgets | Medium | Low | Tag column on keys + budget rule extension |
| 19 | Config hot-reload for deployments | Medium | Medium | Background job + DB-stored model list |
| 20 | Guardrail framework | Medium | Medium | Pre/post call hook interface; providers pluggable |
| 21 | LangFuse / LangSmith callback | Medium | Medium | Callback-style hook shim over existing OTel data |
| 22 | Cold storage / log archival | Medium | Low | Background job + S3 writer |
| 23 | Session-based rate limits | Medium | Medium | Session ID propagation + per-session counters |
| 24 | Prompt caching (Anthropic cache_control) | Medium | Low | Request transform before upstream send |
| 25 | Kubernetes / Helm charts | Medium | Medium | Ops work; no code changes to modelrouter |
| 26 | Key expiration / auto-rotation | Medium | Low | TTL column + background expiry job |
| 27 | MCP server registry | Medium | High | New subsystem; growing importance |
| 28 | Policy engine (declarative cascading) | Medium | High | New abstraction layer over budget rules |
| 29 | SSO / OIDC | Low | High | Third-party IdP integration |
| 30 | SCIM provisioning | Low | High | IdP webhook + user sync |
| 31 | Batch API (`/v1/batches`) | Low | Medium | New route + async job; background cost tracking |
| 32 | Request queuing under rate limits | Low | Medium | Async queue per user |
| 33 | Shadow traffic routing | Low | High | Parallel dispatch; response discarding |
| 34 | Image generation endpoint | Low | Medium | New route + DALL-E adapter |
| 35 | Audio (Whisper / TTS) | Low | Medium | New routes + provider adapters |
| 36 | Billing integrations (Stripe, Lago) | Low | High | External billing API + usage event push |
| 37 | Agent endpoints | Low | High | New execution model |
| 38 | Vector stores / RAG endpoints | Low | High | New subsystem |
| 39 | Realtime WebSocket API | Low | High | New connection model |
| 40 | Responses API (`/v1/responses`) | Low | Low | Passthrough route; provider translates |

---

_End of report_
