# Rename + Docker Distribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename the repo from `tokenomics` to `modelrouter`, update all internal references, and add multi-arch Docker image publishing to GHCR as part of the release workflow.

**Architecture:** Phase 1 updates internal references and README (no code changes). Phase 2 updates the Dockerfile to accept a `FEATURES` build-arg and adds a `docker` job to the release workflow that builds 4 multi-arch image variants and pushes them to GHCR (private).

**Tech Stack:** GitHub Actions, Docker Buildx, QEMU, GHCR (`ghcr.io`), Rust/Cargo feature flags, distroless base image.

---

## Pre-Flight: Manual GitHub Steps (do before any code changes)

These cannot be scripted — they must be done in the GitHub UI first.

- [ ] Go to `github.com/keithmackay/modelrouter` (the Python project) → Settings → Rename repo to `modelrouter_py`
- [ ] Go to `github.com/keithmackay/tokenomics` (this Rust project) → Settings → Rename repo to `modelrouter`
- [ ] After renaming, check Settings → Secrets / Environments / Webhooks on the new `modelrouter` repo — GitHub does NOT automatically remap these; verify they still exist and point correctly.

---

## Phase 1 — Internal Reference Updates

### Task 1: Update README.md

**Files:**
- Modify: `README.md:3,57-58,69-76`

This task fixes the badge URL and clone instructions, then replaces the local-build Docker section with the full GHCR pull table.

- [ ] **Step 1: Fix badge URL (line 3)**

Change:
```markdown
![Release](https://img.shields.io/github/actions/workflow/status/keithmackay/tokenomics/release.yml?label=release)
```
To:
```markdown
![Release](https://img.shields.io/github/actions/workflow/status/keithmackay/modelrouter/release.yml?label=release)
```

- [ ] **Step 2: Fix clone URL (lines 57-59)**

Change:
```bash
git clone https://github.com/keithmackay/tokenomics.git
cd tokenomics
```
To:
```bash
git clone https://github.com/keithmackay/modelrouter.git
cd modelrouter
```

- [ ] **Step 3: Replace Docker section (lines 69-76)**

Change the existing Docker block:
```markdown
**Docker:**

```bash
docker build -t modelrouter .
docker run -v /host/config:/config -v /host/data:/data \
  -e MODELROUTER_CONFIG=/config/config.toml \
  -p 8080:8080 modelrouter serve
```
```

To:
```markdown
**Docker (from GHCR):**

| Image | Features |
|---|---|
| `ghcr.io/keithmackay/modelrouter:latest` | SQLite only |
| `ghcr.io/keithmackay/modelrouter:latest-otel` | + OpenTelemetry |
| `ghcr.io/keithmackay/modelrouter:latest-postgres` | + PostgreSQL |
| `ghcr.io/keithmackay/modelrouter:latest-full` | All features (OTel + Postgres + Bedrock + Prometheus) |

```bash
docker pull ghcr.io/keithmackay/modelrouter:latest
docker run \
  -v /host/config:/config \
  -v /host/data:/data \
  -e MODELROUTER_CONFIG=/config/config.toml \
  -p 8080:8080 \
  ghcr.io/keithmackay/modelrouter:latest
# -p 8080:8080 maps to server.port in config.toml (default: 8080)
```

**Build from source:**

```bash
git clone https://github.com/keithmackay/modelrouter.git
cd modelrouter
cargo build --release
# Binary is at target/release/modelrouter
```
```

- [ ] **Step 4: Verify no remaining `tokenomics` references in README.md**

```bash
grep -n "tokenomics" README.md
```
Expected: no output.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: update README for modelrouter rename and GHCR Docker distribution"
```

---

### Task 2: Update remaining internal references

**Files:**
- Modify: `CONTRIBUTING.md:14-15`
- Modify: `docs/local-setup.md:24,37`
- Modify: `deploy/helm/modelrouter/Chart.yaml:11`
- Modify: `docs/superpowers/plans/2026-04-05-phase16-helm-charts.md:146`
- Modify: `docs/superpowers/plans/2026-04-05-phase18-declarative-policy-engine.md:774`
- Modify: `session_stats.md` (project name column — tracking file only, not source)

- [ ] **Step 1: Update CONTRIBUTING.md (lines 14-15)**

Change:
```
git clone https://github.com/keithmackay/tokenomics.git
cd tokenomics
```
To:
```
git clone https://github.com/keithmackay/modelrouter.git
cd modelrouter
```

- [ ] **Step 2: Update docs/local-setup.md (lines 24, 37)**

Line 24 — change:
```
cd /Users/Keith.MacKay/Projects/tokenomics
```
To:
```
cd /Users/Keith.MacKay/Projects/modelrouter
```

Line 37 — change:
```
export PATH="$PATH:/Users/Keith.MacKay/Projects/tokenomics/target/release"
```
To:
```
export PATH="$PATH:/Users/Keith.MacKay/Projects/modelrouter/target/release"
```

- [ ] **Step 3: Update deploy/helm/modelrouter/Chart.yaml (line 11)**

Change:
```yaml
home: https://github.com/keithmackay/tokenomics
```
To:
```yaml
home: https://github.com/keithmackay/modelrouter
```

- [ ] **Step 4: Update plan docs**

In `docs/superpowers/plans/2026-04-05-phase16-helm-charts.md` line 146:
```yaml
home: https://github.com/keithmackay/tokenomics
```
→
```yaml
home: https://github.com/keithmackay/modelrouter
```

In `docs/superpowers/plans/2026-04-05-phase18-declarative-policy-engine.md` line 774:
```
cd /Users/Keith.MacKay/Projects/tokenomics
```
→
```
cd /Users/Keith.MacKay/Projects/modelrouter
```

> **Note on session_stats.md:** It contains `tokenomics` in historical session rows. These represent actual past sessions — leave them as-is. No edit needed.

- [ ] **Step 5: Verify no remaining tokenomics references in source files**

```bash
grep -rn "tokenomics" --include="*.md" --include="*.toml" --include="*.yaml" --include="*.yml" --include="*.rs" .
```
Expected: only matches inside `session_stats.md` (historical data) and this plan file itself. No matches in source, config, or workflow files.

- [ ] **Step 6: Commit**

```bash
git add CONTRIBUTING.md docs/local-setup.md deploy/helm/modelrouter/Chart.yaml \
  docs/superpowers/plans/2026-04-05-phase16-helm-charts.md \
  docs/superpowers/plans/2026-04-05-phase18-declarative-policy-engine.md
git commit -m "docs: update all tokenomics → modelrouter references after repo rename"
```

---

### Task 3: Rename local folders

These shell commands must be run after the GitHub rename and after all the commits above are pushed.

- [ ] **Step 1: Push current commits**

```bash
git push
```

- [ ] **Step 2: Rename local folders**

The Python project may or may not exist locally — guard against that:

```bash
# Only rename the Python project folder if it exists locally
if [ -d ~/Projects/modelrouter ]; then
  mv ~/Projects/modelrouter ~/Projects/modelrouter_py
else
  echo "~/Projects/modelrouter not found locally — skipping Python project rename"
fi
mv ~/Projects/tokenomics ~/Projects/modelrouter
```

- [ ] **Step 3: Update the remote URL in the renamed folder**

```bash
cd ~/Projects/modelrouter
git remote set-url origin https://github.com/keithmackay/modelrouter.git
git remote -v
```
Expected:
```
origin  https://github.com/keithmackay/modelrouter.git (fetch)
origin  https://github.com/keithmackay/modelrouter.git (push)
```

- [ ] **Step 4: Verify git still works**

```bash
git status
git log --oneline -3
```
Expected: clean working tree, recent commits visible.

---

## Phase 2 — Docker Distribution

### Task 4: Update Dockerfile to accept FEATURES build-arg

**Files:**
- Modify: `Dockerfile`

The `ARG FEATURES=""` must be declared **inside** the builder stage (after the `FROM` line), not before it — Docker `ARG` before `FROM` is only in scope for `FROM` itself.

- [ ] **Step 1: Update Dockerfile**

Replace the full builder stage with:

```dockerfile
# ── Builder stage ────────────────────────────────────────────────────────────
FROM rust:1.91-slim AS builder

ARG FEATURES=""

WORKDIR /build

# Install build dependencies for SQLite bundled feature
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./

# Create a stub src/main.rs to pre-build dependencies with the same feature set
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs
RUN if [ -n "$FEATURES" ]; then \
      cargo build --release --features "$FEATURES" || true; \
    else \
      cargo build --release || true; \
    fi
RUN rm -rf src

# Copy full source and build for real
COPY . .
RUN if [ -n "$FEATURES" ]; then \
      cargo build --release --features "$FEATURES"; \
    else \
      cargo build --release; \
    fi
```

Leave the runtime stage (`FROM gcr.io/distroless/cc-debian12`) unchanged.

- [ ] **Step 2: Test the default build locally**

```bash
docker build -t modelrouter:test-default .
```
Expected: build succeeds, no `--features` flag used.

- [ ] **Step 3: Test the otel variant locally**

```bash
docker build --build-arg FEATURES=otel -t modelrouter:test-otel .
```
Expected: build succeeds, `--features otel` passed to cargo.

- [ ] **Step 4: Verify the binary runs**

```bash
docker run --rm modelrouter:test-default --help
```
Expected: prints usage/help text and exits 0. (`--version` is not guaranteed to be implemented; `--help` is always handled by clap with a clean exit.)

- [ ] **Step 5: Commit**

```bash
git add Dockerfile
git commit -m "build: add FEATURES build-arg to Dockerfile for multi-variant images"
```

---

### Task 5: Add docker job to release workflow

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Append the docker job to .github/workflows/release.yml**

Add the following after the closing of the existing `release:` job:

```yaml
  docker:
    name: Docker (${{ matrix.variant }})
    needs: [build, release]
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    env:
      IMAGE_NAME: ${{ github.repository }}
    strategy:
      matrix:
        include:
          - variant: default
            features: ""
            tag_suffix: ""
          - variant: otel
            features: "otel"
            tag_suffix: "-otel"
          - variant: postgres
            features: "postgres"
            tag_suffix: "-postgres"
          - variant: full
            features: "otel,postgres,bedrock,prometheus"
            tag_suffix: "-full"
    steps:
      - uses: actions/checkout@v4

      - name: Set up QEMU
        uses: docker/setup-qemu-action@v3

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Build and push
        uses: docker/build-push-action@v6
        with:
          context: .
          platforms: linux/amd64,linux/arm64
          push: true
          build-args: FEATURES=${{ matrix.features }}
          cache-from: type=gha,scope=${{ matrix.variant }}
          cache-to: type=gha,mode=max,scope=${{ matrix.variant }}
          tags: |
            ghcr.io/${{ env.IMAGE_NAME }}:${{ github.ref_name }}${{ matrix.tag_suffix }}
            ghcr.io/${{ env.IMAGE_NAME }}:latest${{ matrix.tag_suffix }}
```

- [ ] **Step 2: Validate the workflow YAML is well-formed**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" && echo "YAML valid"
```
Expected: `YAML valid`

> **Note:** This validates YAML structure only — it does not catch GitHub Actions-specific schema errors (wrong key names, bad indentation of job blocks, etc.). After validation, visually inspect the indentation of the new `docker:` job block to confirm it is at the same level as `build:` and `release:`.

- [ ] **Step 3: Review the full workflow to confirm job ordering**

```bash
grep -A2 "needs:" .github/workflows/release.yml
```
Expected output shows:
- `release` job needs `build`
- `docker` job needs `[build, release]`

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add multi-arch Docker image publishing to GHCR on release"
```

---

### Task 6: Push and verify

- [ ] **Step 1: Push all commits**

```bash
git push
```

- [ ] **Step 2: Dry-run verify by checking the workflow syntax on GitHub**

Open `https://github.com/keithmackay/modelrouter/actions` — confirm the Release workflow is listed and shows no syntax errors in the UI.

- [ ] **Step 3: (Optional) Trigger a test release**

To test the full pipeline, create a test tag:

```bash
git tag v0.1.1-test
git push origin v0.1.1-test
```

Watch the Actions run. Verify:
- `build` job produces 4 binaries
- `release` job creates a GitHub Release
- `docker` job runs 4 matrix variants
- GHCR packages appear at `https://github.com/keithmackay?tab=packages` with `modelrouter` name

Delete the test tag after verification:

```bash
git push origin --delete v0.1.1-test
```

---

## Releasing a New Version

This section covers how to publish a release after the workflow is set up. Repeat these steps for every future release.

### Step 1: Tag the release

Tags must follow the `v*` pattern to trigger the workflow. Use semantic versioning:

```bash
# Stable release
git tag v0.2.0
git push origin v0.2.0

# Pre-release (will NOT receive a `latest` tag — see note below)
git tag v0.2.0-beta.1
git push origin v0.2.0-beta.1
```

> **`latest` tag behavior:** The workflow applies `latest`/`latest-otel`/etc. only when the tag contains no `-` (i.e., stable releases). Pre-release tags like `v0.2.0-beta.1` get versioned tags only.

### Step 2: Watch the Actions run

Open `https://github.com/keithmackay/modelrouter/actions` and confirm all three jobs complete:

| Job | What it does |
|-----|-------------|
| `build` (×4) | Compiles binaries for Linux x86_64, Linux arm64, macOS arm64, macOS x86_64 |
| `release` | Creates a GitHub Release with those 4 binaries attached |
| `docker` (×4) | Builds and pushes all 4 image variants to GHCR |

If any `docker` job fails but `build` and `release` succeed, re-run just the failed matrix entry from the Actions UI — no need to re-tag.

### Step 3: Verify the images were pushed

After the workflow completes, the following tags will exist on GHCR (using `v0.2.0` as an example):

```
ghcr.io/keithmackay/modelrouter:v0.2.0
ghcr.io/keithmackay/modelrouter:v0.2.0-otel
ghcr.io/keithmackay/modelrouter:v0.2.0-postgres
ghcr.io/keithmackay/modelrouter:v0.2.0-full
ghcr.io/keithmackay/modelrouter:latest
ghcr.io/keithmackay/modelrouter:latest-otel
ghcr.io/keithmackay/modelrouter:latest-postgres
ghcr.io/keithmackay/modelrouter:latest-full
```

Check them at: `https://github.com/keithmackay?tab=packages` or `https://github.com/keithmackay/modelrouter/pkgs/container/modelrouter`

---

## First-Time Package Privacy Setup (One-Time Manual Step)

**GHCR packages are PUBLIC by default.** The workflow cannot set visibility — this must be done manually in the GitHub UI after the first push.

### How to make the package private

1. Go to `https://github.com/keithmackay/modelrouter/pkgs/container/modelrouter`
2. Click **Package settings** (gear icon, right side)
3. Scroll to **Danger Zone** → **Change visibility**
4. Select **Private** → confirm

> **This is a single package** (`modelrouter`) even though there are multiple tagged variants (`:latest`, `:latest-otel`, etc.). Setting the package to private covers all tags at once. You do not need to repeat this per tag or per variant.

### Granting pull access to other users or machines

Once private, only repo members with at least `read` access can pull the image. To pull on a machine:

```bash
# Authenticate with a GitHub Personal Access Token (PAT) that has `read:packages` scope
echo "<YOUR_PAT>" | docker login ghcr.io -u <github-username> --password-stdin

# Then pull normally
docker pull ghcr.io/keithmackay/modelrouter:latest
```

---

## Completion Checklist

- [ ] GitHub repos renamed (manual)
- [ ] All `tokenomics` → `modelrouter` references updated in source/docs/config
- [ ] README Docker section updated with GHCR pull instructions
- [ ] Local folders renamed, remote URL updated
- [ ] Dockerfile accepts `FEATURES` build-arg with correct `ARG` scope
- [ ] Release workflow has `docker` job with 4 variants, multi-arch, GHA cache
- [ ] YAML validated, job ordering confirmed (`docker` needs `[build, release]`)
- [ ] All commits pushed to `keithmackay/modelrouter`
- [ ] Run `cargo clean` to reclaim disk space (`target/` can reach 48+ GB)
