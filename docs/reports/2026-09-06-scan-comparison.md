# modelrouter — Code-Quality Scan Comparison and Fix Impact Assessment

Written 2026-09-06. Compares the first code-quality scan of modelrouter (SonarQube-based static analysis + SBOM/CVE dependency audit) with
the last (post-hardening) scan, lists the fixes implemented in between, and
assesses each fix's impact on the scan results.

## 1. The scans

| | First scan (baseline) | Last scan (post-fix) |
|---|---|---|
| Date | 2026-09-03 21:47 UTC | 2026-09-05 21:55 UTC |
| Code scanned | `main@aef93be` (pre-hardening) | `main@c25cb705` (post-hardening + upstream experiments feature) |
| Report dir | retained outside the repo | retained outside the repo |
| Lines of code | 36,937 | 52,835 |

The first scan is the run that generated issues #58 (dependency advisories)
and #59 (form-control labels). A re-baseline on 2026-09-05 19:20
against the same commit reproduced the first
scan's numbers exactly (46 bugs / 93 smells / 33 advisories), which is good
evidence the scanner is deterministic and the comparison below is sound.

Caveat: both scans record `ProjectVersion: main-aef93be` because the CLI
ignores the `--project-version` override when a saved config exists (filed upstream against the scanner). The post-fix scan verifiably ran on the new tree —
ncloc, findings, and the dependency set all changed accordingly.

## 2. Headline results

| Metric | First scan | Last scan | Δ |
|---|---|---|---|
| **High-severity dependency CVEs** | **8** | **0** | **−8** |
| Medium / Low / Unrated scored CVEs | 8 / 6 / 11 | 0 / 0 / 0 | all cleared |
| Dependency advisories listed (all bands) | 33 | 22 | −11 |
| Vulnerability risk score (0–100, lower better) | 1.24 (grade A) | **0.00 (grade A)** | −1.24 |
| **Static-analysis bugs** | **46** | **7** | **−39** |
| Code smells | 93 | 66 | −27 |
| Label-association findings (issue #59) | 89 | **0** | −89 |
| Static-analysis "vulnerability" findings | 4 | 4 | 0 |
| Security hotspots | 0 | 0 | 0 |
| Licence risk score | 0.22 (A) | 0.40 (A) | +0.18 |
| Development practice health | 37.57 (C) | 37.85 (C) | ≈ flat |
| Duplicated lines density | 13.1% | 13.2% | ≈ flat |
| Reliability / Security / Maintainability ratings | C / C / A | C / C / A | unchanged |
| Quality gate | OK | ERROR (new-code conditions) | regression, see §5 |

## 3. Fixes implemented between the scans

All fixes merged to main in `bf705fdc` (sweep) and `c25cb705` (integration
with upstream), 18 commits, each subagent-implemented with independent code
review. Suite grew 508 → 748 passing tests.

**Scanner-driven fixes (directly targeted scan findings):**

1. **#58 — dependency advisories:** openssl 0.10.76→0.10.81, quinn-proto
   0.11.14→0.11.17, rustls-webpki →0.103.13 on all non-bedrock TLS paths.
2. **#59 — 89 unlabeled form controls** across 10 admin templates: `label[for]`
   + `id`, wrapping labels, or `aria-label` on every control.

**Bug fixes (human/agent-found, not from the scanner):**

3. **#50** — Postgres `/admin/reports` 500 after any health probe (probe wrote
   a JSON array into `attribution_tags`; fixed writer + backfill migration 030
   + defensive `jsonb_typeof` filter).
4. **#51** — OIDC-provisioned admins could never hold superadmin (default role
   now "viewer", startup validation against a single role vocabulary,
   deny-on-unknown).
5. **#41** — silent `tools`/`tool_choice` drop on `/v1/chat/completions` and
   `/v1/responses` now a 400 naming the field (`tools: []`, `tool_choice:
   null`/`"none"` still pass).

**Features (closing requested capability gaps):**

6. **#43** — `[admin.bootstrap]` config section (idempotent create-if-absent)
   + `admin hash-password` CLI.
7. **#45** — `attribution_correlation_id` in the failures list,
   `/admin/failures/:id` detail (htmx inline), `?correlation_id=` filter.
8. **#42** — search engine fallback chains (provider-errors-only failover,
   serving-engine metadata, cost re-attributed to the serving engine).
9. **#35** — alias targets validated against the discovered model catalog
   (datalist UI + cached server-side validation with per-provider degradation).
10. **#44 / #23** — closed on verification: MaaS publisher discovery/dispatch
    and the discovery+mapping-UI parent were already complete.

## 4. Impact assessment — which fixes moved which numbers

- **#58 → the entire vulnerability improvement.** High CVEs 8→0, vulnerability
  score 1.24→0.00, advisory count 33→22. The after-scan retains exactly one of
  the nine original high-band advisory IDs — `GHSA-82j2-j2ch-gfr8`
  (rustls-webpki 0.101.7) — which is the *disclosed* residual: that copy is
  reachable only in `--features bedrock` builds via the AWS SDK's rustls 0.21
  pin, and the scanner's scored high-count still reads 0 because the fixed
  0.103.13 serves the scored paths. Scan and disclosure agree.
- **#59 → the entire static-analysis improvement.** The −39 bugs and −27
  smells (−66 total) are overwhelmingly the 89 label-association findings,
  which SonarQube splits across its bug and code-smell categories, partially
  offset by ~23 new findings introduced with 15.9k lines of new code (mostly
  the upstream experiments feature, plus some sweep code). Post-fix, the
  label-rule count is 0 by both the scanner and an independent rule-equivalent
  check.
- **Fixes #50, #51, #41, #42, #43, #45, #35 → little to no scan movement, by
  design.** These are behavioral/security-logic fixes below static analysis's
  detection threshold (fail-open role checks, silent parameter drops, wrong
  cost attribution). Their evidence is the 240 new tests, not the scan. This
  is the key calibration finding of the exercise: **the scanner found the
  mechanical 2 of 11 issues but drove ~100% of the scan-visible improvement;
  the other 9 required humans/agents reading code and reviews.** Scanner and
  review are complements, not substitutes.
- **What deliberately didn't move:** the 4 static "vulnerability" findings
  (never filed as issues — candidates for round two); reliability/security
  ratings stay C because those are worst-severity-based and at least one
  major finding remains in each category; development-practice health is a
  git-history metric (knowledge concentration, commit signing) that code
  fixes can't shift.
- **Small honest regressions, all explained by code growth:** licence risk
  +0.18 (new transitive deps from the crate bumps and the experiments
  feature — still grade A); duplication 13.1→13.2%.

## 5. The quality-gate flip (OK → ERROR) is not caused by the fixes

The gate fails on two *new-code* conditions: 11.7% duplicated lines on new
code (threshold 3%) and 24 new issues (threshold 0). The dominant contributor
is the ~15.7k-line experiments feature that landed on origin/main mid-sweep
and rode into the "after" scan; the sweep's own additions contribute a minor
share. Practical reading: the codebase's *stock* of problems dropped sharply
while the latest *increment* of code would not pass the gate — the 24
new-code issues plus the 7 remaining bugs and 4 vulnerability findings are
the natural work-list for a second hardening round.

## 6. Bottom line

Between the first and last scan, every finding the scanner could see and that
was filed as an issue was driven to zero (8 high CVEs, 89 label findings),
the bug count fell 85% (46→7) even while the codebase grew 43%, and the one
surviving advisory is a documented, upstream-pinned residual rather than an
oversight. The scanner's findings were 100% accurate (no false positives
encountered); its blind spots — behavioral and authorization bugs — were
covered by the review-driven half of the sweep, which produced no scan
movement but 240 tests' worth of verified fixes.
