# Competitor Feature Comparison

> **Scope:** modelrouter (this project) vs. the major LLM gateway, proxy, and routing tools as of mid-2026.
>
> **Tool identity notes:**
> - **modelrouter.app** — thin SaaS aggregation layer over OpenRouter; competes on zero markup pricing
> - **OpenLLM (BentoML)** — self-hosted *serving framework* for open-source models, not a router; many gateway comparisons are N/A by design
> - **LiteLLM Proxy** — de facto open-source reference implementation for LLM gateways; most widely deployed self-hosted option
> - **Portkey** — went fully open-source (Apache 2.0) in March 2026; acquired by Palo Alto Networks in April 2026
> - **Helicone** — observability-first gateway, also written in Rust; architecturally most similar to this project
> - **Kong AI Gateway** — extends Kong API gateway with AI plugins; targets enterprises already running Kong
> - **Cloudflare AI Gateway** — free managed SaaS on Cloudflare edge; no self-hosting
> - **One API** — free MIT-licensed self-hosted proxy dominant in the Chinese market; best coverage of domestic models
> - **Martian** — SaaS-only ML-driven prompt router; not a full gateway
> - **Not Diamond** — client-side routing recommendation layer; complements a gateway rather than replacing it

---

## At a Glance

| | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** | **Martian** | **Not Diamond** |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Type | Self-hosted proxy/router | Self-hosted proxy + SaaS | Self-hosted proxy + SaaS | Managed SaaS + self-hosted | Managed SaaS aggregator | Self-hosted / SaaS (Konnect) | Managed SaaS | Self-hosted proxy | Self-hosted serving framework | Managed SaaS aggregator | Managed SaaS router | Client-side routing layer |
| Open source | Yes (Rust) | Yes (MIT) | Yes (Apache 2.0) | No (enterprise on-prem) | No | Yes (core) | No | Yes (MIT) | Yes (Apache 2.0) | No | No | No |
| Self-hosted | Yes | Yes | Yes | Enterprise only | No | Yes | No | Yes | Yes | No | No | No |
| Free to run | Yes | Yes (OSS) | Yes (OSS) | 10K req/mo free | 5.5% fee on credits | Yes (OSS core) | Yes | Yes | Yes | Pay-as-you-go | Metered | Metered |
| Language | Rust | Python | TypeScript | Rust | — | Go/Lua | — | Go | Python | — | — | — |
| Target audience | Engineering teams, enterprises | Developers to enterprise | Developers to enterprise (PANW backing) | Developers to enterprise | Developers to enterprise | Enterprises on Kong | Cloudflare users | Solo devs, Chinese market | Teams running open-source models | Budget-conscious devs | Cost-reduction-focused teams | Teams wanting client-side routing |

---

## Feature Matrix

### Routing & Model Coverage

| Feature | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** |
|---|---|---|---|---|---|---|---|---|---|---|
| Supported providers | Anthropic, OpenAI, Azure, Gemini, Bedrock, Ollama, generic OpenAI-compat | 100+ providers | 1,600+ models across providers | OpenAI, Anthropic, Azure, Gemini, Bedrock, others | 60+ providers, 500+ models | OpenAI, Anthropic, Azure, Cohere, others | OpenAI, Anthropic, Workers AI, HuggingFace, xAI | OpenAI, Anthropic, Azure, Gemini, DeepSeek, domestic Chinese models | Open-source models (Llama, Qwen, DeepSeek, Mistral, Gemma, Phi4) | All OpenRouter providers |
| Domestic Chinese model support | No | Partial | Partial | No | Partial | No | No | Yes (Qianwen, Wenxin, Hunyuan, etc.) | Via HuggingFace | No |
| Model aliases | Yes (config + DB) | Yes | Yes | No | N/A | Yes | No | Yes | N/A | N/A |
| ML-driven auto-routing | No (rule-based complexity routing) | No | No | No | Yes (NotDiamond) | No | No | No | No | Yes (prompt difficulty + latency) |
| Load balancing pools | Yes (round-robin or weighted) | Yes | Yes | No | Yes | Yes (semantic LB) | No | Yes | No | Unknown |
| Failover chains | Yes (config + DB, cascading) | Yes | Yes (conditional routing) | No | Yes (automatic) | Yes | Yes (retries/fallbacks) | Yes | No | Unknown |
| Circuit breaker | Yes (Closed/Open/HalfOpen per provider) | Yes | Yes | No | Yes | Yes | No | No | No | Unknown |
| Session stickiness | No | No | No | No | Yes (`session_id`) | No | No | No | N/A | Unknown |
| Performance routing params | No | No | No | No | Yes (`preferred_min_throughput`, `preferred_max_latency`) | No | No | No | No | No |
| Routing shortcuts (`:nitro`/`:floor`) | No | No | No | No | Yes | No | No | No | No | No |
| Semantic caching | No (LRU exact-match cache) | No | Yes | No | No | No | Yes (edge cache) | No | No | No |
| OpenAI API compatibility | Yes | Yes | Yes | Yes (proxy mode) | Yes | Yes | Yes | Yes | Yes | Yes |
| Anthropic API compatibility | Yes (native `/v1/messages`) | Via translation | Via translation | Via translation | Via translation | Via translation | Via translation | Via translation | No | Via translation |
| Structured output enforcement | Via provider | Via provider | No | No | Yes (`response_format` JSON schema) | No | No | No | Model-dependent | Via provider |
| Automatic PDF/image parsing | No | No | No | No | Yes | No | No | No | No | No |

### Authentication & Access Control

| Feature | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** |
|---|---|---|---|---|---|---|---|---|---|---|
| API key auth | Yes (SHA-256, shown once) | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Optional | Yes |
| Per-key metadata (project, label, expiry) | Yes | Yes | Yes | Yes | Yes | Yes | No | Yes | No | Unknown |
| Key rotation | Yes | Yes | Yes | Yes | Yes | Yes | No | Yes | No | Unknown |
| Key revocation | Yes | Yes | Yes | Yes | Yes | Yes | No | Yes | No | Unknown |
| Model allow/deny per key | Yes (via budget rules) | Yes | Yes | No | Yes | Yes | No | Yes | No | Unknown |
| Admin RBAC | Yes (superadmin, viewer) | Yes (teams/orgs) | Yes | Yes | Enterprise only | Yes | No | Yes | No | Yes |
| SSO / OIDC | Yes (any OIDC, PKCE) | Yes (enterprise) | Yes | No | Enterprise only | Yes | No | No | No | Unknown |
| BYOK (your provider keys) | Yes (always) | Yes | Yes | Yes | Yes (60+ providers) | Yes | Yes | Yes | Yes (your infra) | Not documented |

### Budget & Cost Management

| Feature | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** |
|---|---|---|---|---|---|---|---|---|---|---|
| Spend tracking | Yes (user, project, model, key) | Yes | Yes | Yes | Yes (per-request cost) | Yes (token metering) | Yes (usage analytics) | Yes | No | Yes |
| Budget scopes | Global, project, user, group, per-key | Per-user, per-team, per-key | Per-user, per-project | Per-user | Per-key | Per-consumer | None | Per-user, per-channel | N/A | Unknown |
| Group-level soft targets | Yes | No | No | No | No | No | No | No | No | No |
| Budget windows | Daily, weekly, monthly, fixed range | Daily, monthly | Daily, monthly | Monthly | Daily, weekly, monthly | Rolling window | None | None | N/A | Unknown |
| USD spend limits | Yes | Yes | Yes | Yes | Yes | No | No | Yes (quota) | No | Unknown |
| Token limits | Yes | Yes | Yes | No | No | Yes | No | Yes | No | Unknown |
| Rate limits (RPM/TPM) | Yes (per rule + global IP) | Yes | Yes | No | Yes | Yes | Yes | Yes | No | Yes |
| Concurrency limits | Yes (`max_concurrent`) | Yes | No | No | No | Yes | No | No | No | Unknown |
| Custom pricing overrides | Yes (`[[pricing]]` in config) | Yes | No | No | No | No | No | No | N/A | N/A |
| Cost archival to S3 | Yes | No | No | No | No | No | No | No | No | No |
| Declarative policy rules (config file) | Yes (`[[policy_rules]]` TOML) | No | No | No | No | Yes (Kong plugins) | No | No | No | No |

### Observability & Reporting

| Feature | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** |
|---|---|---|---|---|---|---|---|---|---|---|
| Prometheus metrics | Yes (`--features prometheus`) | Yes | Yes | No | No | Yes | No | No | No | Unknown |
| OpenTelemetry (traces + metrics + logs) | Yes (`--features otel`, OTLP push) | Yes | Yes | No | No | Yes | No | No | No | No |
| Audit log | Yes (all admin mutations) | Yes | Yes | No | No | Yes | No | No | No | Yes |
| Prompt history / log | Yes (full per-user, detail view) | Yes | Yes (with evals) | Yes | No | No | No | No | No | Unknown |
| Cost dashboard (built-in) | Yes (D3.js, per-user/project/model) | Yes | Yes | Yes | Yes | Yes (Konnect) | Yes | Yes | No | Yes |
| CLI cost/usage reports | Yes (CSV/JSON/table, multi-filter) | No | No | No | No | No | No | No | No | No |
| LangSmith integration | Yes (built-in callbacks) | Yes | Yes | No | No | No | No | No | No | No |
| LangFuse integration | Yes (built-in callbacks) | Yes | Yes | Yes | Yes (Broadcast) | No | No | No | No | No |
| Datadog integration | No (via OTel) | Yes | Yes | No | Yes (Broadcast) | Yes | No | No | No | No |
| W&B Weave integration | No (via OTel) | No | No | No | Yes (Broadcast) | No | No | No | No | No |
| Hook performance metrics | Yes | No | No | No | No | No | No | No | No | No |

### Content Safety & Guardrails

| Feature | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** |
|---|---|---|---|---|---|---|---|---|---|---|
| Request/response guardrails | Yes (pluggable chain: allow/block/replace) | Yes (via callbacks) | Yes | No | No | Yes (plugins) | No | No | No | No |
| OpenAI moderation integration | Yes | Yes | Yes | No | No | No | No | No | No | No |
| Custom guardrail scripts | Yes (subprocess-based) | Yes (custom callbacks) | No | No | No | Yes (custom plugins) | No | No | No | No |
| Data privacy / prompt retention | Local DB (you own it) | Local (self-hosted) | Local (self-hosted) | Logs stored by Helicone | No logging by default | Local (self-hosted) | Minimal logging | Local (self-hosted) | Your infra | Unknown |
| Zero data retention per-request | N/A (all local) | N/A (all local) | N/A (all local) | Yes (enterprise) | Yes (`zdr: true`) | N/A (all local) | No | N/A (all local) | Yes (your infra) | Unknown |
| EU / regional data routing | Via provider config | Via provider config | No | No | Yes (in-region routing) | Via provider config | Yes (Cloudflare regions) | No | Via provider config | Unknown |

### Admin & Developer Experience

| Feature | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** |
|---|---|---|---|---|---|---|---|---|---|---|
| Web UI | Yes (HTMX + D3.js, full dashboard) | Yes | Yes | Yes | Yes | Yes (Konnect) | Yes | Yes | `/chat` UI + BentoCloud | Yes |
| CLI | Yes (full management CLI) | Yes | No | No | No | Yes (`deck`) | No | No | Yes (`openllm`) | No |
| Pipeline hooks (mutate request/response) | Yes (subprocess, per-request) | Yes (custom callbacks) | No | No | No | Yes (plugins) | No | No | No | No |
| Lifecycle hooks | Yes (server start/shutdown) | No | No | No | No | No | No | No | No | No |
| MCP server registry | Yes (register + semantic search) | No | Yes (MCP Gateway, OAuth 2.1) | No | No | Yes (A2A + MCP) | No | No | No | No |
| Response caching | Yes (LRU, configurable TTL) | Yes | Yes (semantic) | No | No | Yes | Yes (edge, zero-latency) | No | No | Unknown |
| Retry with backoff | Yes | Yes | Yes | No | No (provider-side) | Yes | Yes | No | No | Unknown |
| Prompt versioning / evals | No | No | Yes | Yes (evals) | No | No | No | No | No | No |
| DB backend | SQLite or PostgreSQL | SQLite or PostgreSQL | PostgreSQL | Managed (SaaS) | Managed | Managed | Managed | MySQL / SQLite | N/A | Managed |
| Deployment | Binary, Docker, Docker Compose, systemd/launchd | Docker, Kubernetes, binary | Docker, Kubernetes | SaaS / enterprise on-prem | SaaS only | Docker, Kubernetes, Konnect | SaaS only | Docker, binary | Local, Docker, K8s, BentoCloud | SaaS only |
| Single static binary | Yes | No (Python runtime) | No (Node runtime) | No | N/A | No | N/A | No | No | N/A |
| Custom CA / corporate proxy support | Yes (Zscaler, Netskope) | Yes | No | No | No | Yes | No | No | No | No |

### Enterprise & Compliance

| Feature | **this project** | **LiteLLM** | **Portkey** | **Helicone** | **OpenRouter** | **Kong AI GW** | **Cloudflare AI GW** | **One API** | **OpenLLM** | **modelrouter.app** |
|---|---|---|---|---|---|---|---|---|---|---|
| SOC 2 | Self-hosted (you own posture) | Self-hosted | PANW security ecosystem | SOC 2 + HIPAA (Team+) | SOC 2 Type 2 (self-reported) | Yes | Yes | Self-hosted | Self-hosted | Unknown |
| Audit trail | Yes | Yes | Yes | No | No | Yes | No | No | No | Yes |
| SSO/SAML | Yes (OIDC + PKCE) | Enterprise | Yes | No | Enterprise | Yes | No | No | No | Unknown |
| Private / air-gap deployment | Yes | Yes | Yes | Enterprise only | No | Yes | No | Yes | Yes | No |
| Volume discounts | N/A | Enterprise | PANW enterprise | Team+ plan | $5K+/mo | Enterprise (Konnect) | Cloudflare plans | N/A | N/A | Unknown |

---

## ML-Driven Routing: A Different Category

Two tools occupy a distinct "intelligent routing" niche — they recommend which model to call rather than proxying the request themselves. Both are complementary to this project rather than direct replacements.

| | **Martian** | **Not Diamond** |
|---|---|---|
| Type | SaaS cloud router | Client-side routing SDK |
| How it works | Routes each prompt to cheapest capable model; proxy hop in critical path | Routes locally after API call; no proxy; request never touches their servers |
| Claimed savings | 20–97% cost reduction | Cheaper than cheapest LLM in token fees |
| Self-hosted | No (VPC enterprise option) | No |
| Custom training | No | Yes (train on your own data) |
| Languages | REST | Python, TypeScript, REST |
| Best paired with | Any gateway as the proxy layer | Any gateway as the proxy layer |
| Unique trait | Accenture-backed; ~$1.3B valuation; learnable routing | 100–200ms decision latency only; routing stays client-side |

---

## What Competitors Have That This Project Lacks

| Gap | Who has it | Notes |
|---|---|---|
| 500+ models out of the box | OpenRouter, LiteLLM, Portkey | This project requires manually adding provider credentials |
| ML-driven auto-routing | OpenRouter (NotDiamond), modelrouter.app, Martian, Not Diamond | Rule-based complexity routing exists but is not ML-driven |
| Semantic caching | Portkey | Cache semantically similar prompts, not just exact matches |
| Session stickiness | OpenRouter | Route multi-turn conversation to same provider instance |
| Performance routing params | OpenRouter | Per-request `preferred_min_throughput` / `preferred_max_latency` |
| Automatic PDF/image parsing for non-multimodal models | OpenRouter | Transparent multimodal upgrade |
| EU in-region data residency routing | OpenRouter, Cloudflare | Route traffic to EU providers only |
| Free managed tier (no infra required) | OpenRouter, Cloudflare, modelrouter.app | New users must deploy and manage their own binary |
| Semantic load balancing | Kong AI Gateway | Route to provider based on semantic similarity of prompt to specialization |
| A2A (agent-to-agent) protocol support | Kong AI Gateway | Native A2A + MCP in one product |
| Prompt versioning and evals | Portkey, Helicone | Track prompt iterations and run evaluations against them |
| W&B Weave / Datadog Broadcast webhooks | OpenRouter | Push to Datadog/W&B without OTel sidecar |
| Structured output enforcement (JSON schema) | OpenRouter | `response_format` validation across all models |
| HuggingFace model hub integration | OpenLLM | Pull any open-source model by name |
| Autoscale-to-zero for open-source models | OpenLLM + BentoCloud | Horizontal scaling for variable open-source model workloads |

---

## What This Project Has That Competitors Lack

| Advantage | Who's missing it | Notes |
|---|---|---|
| Multi-scope budget enforcement (5 levels) | All competitors | Global + project + user + group + per-key, each with independent windows, token/USD/RPM/concurrency limits |
| Group-level soft spend targets | All competitors | Informational team spend targets that never block requests |
| Per-request pipeline hooks (mutate request/response) | LiteLLM (callbacks only), Kong (plugins), others missing | Subprocess-based middleware extensible without code changes or plugins |
| Lifecycle server hooks (start/shutdown) | All competitors | Trigger scripts on server lifecycle events |
| `X-No-Log: true` per-request prompt logging opt-out | All competitors | Skip prompt history and callbacks while preserving cost tracking for budget enforcement |
| DB-managed outbound webhook callbacks with admin UI + CLI | LiteLLM has config-file callbacks; others missing | Register webhooks via `modelrouter webhook add`; fires after each completion |
| `:fastest` / `:cheapest` routing shortcuts | All competitors | Single-keyword routing to operator-configured fastest or cheapest model |
| Built-in pricing for Chinese providers (DeepSeek, Qwen, Doubao) | Most Western tools | Native support without third-party plugins |
| MCP server registry with semantic search | Portkey/Kong have MCP gateway but not a registry | Register and discover MCP tools by cosine similarity |
| Native OTel (traces + metrics + logs, OTLP push) | LiteLLM, Kong also have OTel; others missing | Push to any OTLP backend without a sidecar; others use webhooks |
| Prometheus scrape endpoint | LiteLLM, Kong also have it; others missing | Standard `/metrics` endpoint |
| Full management CLI | LiteLLM has partial; others minimal | Fully scriptable user/key/budget/report management — no UI required |
| LangSmith + LangFuse built-in callbacks | LiteLLM has these; others missing | Per-request trace dispatch with no external gateway |
| Pluggable content guardrail chain (block/replace/allow) | LiteLLM (callbacks), Kong (plugins) have it; others missing | Per-request content safety with OpenAI moderation or custom subprocess |
| Native Anthropic `/v1/messages` endpoint | All competitors translate | Pass-through without protocol translation overhead |
| OIDC with PKCE for admin SSO | LiteLLM enterprise, Portkey have it; others missing or enterprise-only | Standards-compliant SSO not gated behind enterprise tier |
| Custom CA / corporate proxy support | LiteLLM has it; others missing | Works behind Zscaler, Netskope, corporate TLS inspection |
| S3 cost log archival | All competitors | Long-term cost data retention without database bloat |
| Declarative TOML policy rules | All competitors | Zero-DB policy enforcement; rules evaluated before DB at startup |
| Prompt history with full detail view | LiteLLM and Portkey have it; others missing | Complete per-user conversation log with per-prompt detail view |
| Audit log for all admin mutations | LiteLLM and Portkey have it; others missing | Actor + action + resource + timestamp for every admin change |
| Single static binary | All competitors | No Python/Node runtime; trivial to deploy, upgrade, and air-gap |
| systemd/launchd service install via CLI | All competitors | `modelrouter install-service` wires up OS service in one command |
| SQLite + PostgreSQL (both supported) | LiteLLM also has both; others pick one | Start with SQLite, migrate to PostgreSQL for scale |

---

## Positioning Summary

**Use this project when:**
- Full data sovereignty is required (all prompts, keys, and costs stay on your infrastructure)
- You need granular multi-scope budget enforcement across teams, projects, and keys
- You want extensible middleware (pipeline hooks, guardrails, callbacks) without deploying plugins or forking code
- Production-grade observability (OTel, Prometheus, LangSmith, LangFuse) must be native
- A team needs per-user/group spend attribution and prompt auditing
- Compliance or air-gap requirements rule out SaaS intermediaries
- You want a single static binary with no runtime dependencies

**Use LiteLLM when:**
- You want the broadest provider coverage (100+ providers) with the largest open-source community
- Python ecosystem integration is important
- You need cross-team chargeback dashboards out of the box

**Use Portkey when:**
- You want full-stack LLMOps (prompt versioning, evals, guardrails, MCP gateway)
- Palo Alto Networks security ecosystem integration is a future requirement
- 1,600+ model support is needed without manual configuration

**Use Helicone when:**
- You are primarily observability-focused and want a lightweight Rust gateway
- HIPAA compliance (Team+ plan) is required alongside gateway features

**Use OpenRouter when:**
- Instant access to 500+ models with zero infrastructure is the priority
- ML-driven auto-routing (NotDiamond) and performance-parameter routing are needed
- EU data residency or zero data retention per-request are hard requirements

**Use Kong AI Gateway when:**
- Your organization already runs Kong for non-AI API traffic
- A2A protocol and MCP gateway in a single enterprise control plane are needed

**Use Cloudflare AI Gateway when:**
- You are already on Cloudflare and want zero-configuration edge caching and analytics
- There are no per-user budget or self-hosting requirements

**Use One API when:**
- You need domestic Chinese model support (Qianwen, Wenxin, Hunyuan, Doubao)
- Simplicity and minimal footprint matter more than enterprise features

**Use OpenLLM when:**
- You need to self-host open-source models with autoscale-to-zero
- You are not routing between commercial API providers

**Layer Martian or Not Diamond on top of any gateway when:**
- You want ML-driven model selection to reduce costs without changing your gateway
- Not Diamond specifically: client-side routing is preferred to avoid a proxy hop
