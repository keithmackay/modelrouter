# Design: Repo Rename + Docker Distribution

**Date:** 2026-04-06
**Status:** Approved

---

## Overview

Two sequential phases:

1. **Rename** — rename the existing Python project to `modelrouter_py` and this Rust project from `tokenomics` to `modelrouter`, updating all internal references.
2. **Docker distribution** — extend the release workflow to build and publish multi-arch Docker images to GHCR (private) for each feature variant.

---

## Phase 1 — Rename

### GitHub repo renames (manual, done in GitHub UI before code changes)

| Current name | New name |
|---|---|
| `keithmackay/modelrouter` (Python) | `keithmackay/modelrouter_py` |
| `keithmackay/tokenomics` (Rust) | `keithmackay/modelrouter` |

GitHub automatically redirects old URLs after a rename, so existing clones continue to work. Note: GitHub secrets, environments, and any configured webhooks under the old repo name are **not** automatically remapped — review these after renaming.

### Local folder renames (done in shell after GitHub rename)

```bash
mv ~/Projects/modelrouter ~/Projects/modelrouter_py
mv ~/Projects/tokenomics ~/Projects/modelrouter
```

### Internal reference updates (in this repo)

Seven files reference `tokenomics` and must be updated to `modelrouter`:

| File | Change |
|---|---|
| `README.md` | Clone URL, badge URL, install instructions |
| `CONTRIBUTING.md` | Repo URL reference |
| `docs/local-setup.md` | Clone URL or path references |
| `deploy/helm/modelrouter/Chart.yaml` | Source URL or annotation |
| `docs/superpowers/plans/2026-04-05-phase18-declarative-policy-engine.md` | Repo reference |
| `docs/superpowers/plans/2026-04-05-phase16-helm-charts.md` | Repo reference |
| `session_stats.md` | Project name reference (tracking file, not source) |

### README.md additions (Docker section)

The existing Docker section shows only a local `docker build`. It will be expanded to include:

- GHCR pull instructions for each variant
- Variant table (default, otel, postgres, full) with feature descriptions
- Volume mount and environment variable reference
- Note that `-p 8080:8080` maps to `server.port` in `config.toml`

---

## Phase 2 — Docker Distribution

### Image variants

| Tag suffix | Cargo features | Use case |
|---|---|---|
| _(none)_ | `[]` | SQLite only, minimal footprint |
| `-otel` | `otel` | + OpenTelemetry traces/metrics/logs |
| `-postgres` | `postgres` | + PostgreSQL backend |
| `-full` | `otel,postgres,bedrock,prometheus` | All optional features |

`s3-archival` is intentionally excluded from `full` — it is an unimplemented stub with no dependencies and no runtime behavior.

### Tagging scheme (Convention A)

For a `v0.2.0` release:

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

Tags are derived from `github.ref_name`, which is the short tag name (e.g. `v0.2.0`) on tag-push triggers. The workflow only fires on `v*` tags, so this is safe. If a `workflow_dispatch` trigger is ever added, tag generation must be revisited.

### Architecture

Each image tag is a multi-arch manifest covering:
- `linux/amd64` (x86_64)
- `linux/arm64` (aarch64)

Built with `docker buildx` + QEMU emulation on GitHub-hosted runners.

### Dockerfile changes

`ARG FEATURES=""` is declared **inside the builder stage** (after `FROM rust:1.91-slim AS builder`) so it is in scope for the `RUN` commands. Both the stub pre-build step and the real build step pass `--features`:

```dockerfile
FROM rust:1.91-slim AS builder

ARG FEATURES=""

WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./

# Pre-build dependencies with the same feature set to maximise layer cache reuse
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs
RUN cargo build --release \
    $([ -n "$FEATURES" ] && echo "--features $FEATURES" || true) || true
RUN rm -rf src

COPY . .
RUN cargo build --release \
    $([ -n "$FEATURES" ] && echo "--features $FEATURES" || true)
```

The runtime stage is unchanged (distroless, copies the binary).

### GitHub Actions workflow changes

A new `docker` job is added to `.github/workflows/release.yml`. It depends on both `build` and `release` so images are only pushed after the GitHub Release is successfully created. The image name is derived from `github.repository` (owner/repo) to avoid hardcoding.

GHA cache (`type=gha`) is used on `docker/build-push-action` to avoid redundant Rust compilation across the 4-variant matrix.

```yaml
docker:
  name: Docker ${{ matrix.variant }}
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
    - uses: docker/setup-qemu-action@v3
    - uses: docker/setup-buildx-action@v3
    - uses: docker/login-action@v3
      with:
        registry: ghcr.io
        username: ${{ github.actor }}
        password: ${{ secrets.GITHUB_TOKEN }}
    - uses: docker/build-push-action@v6
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

### GHCR visibility

Packages inherit the repository visibility on first push. Since the repo is private, images will be private automatically. No extra API calls needed.

### README.md Docker section (updated content)

```markdown
**Docker (from GHCR):**

| Image | Features |
|---|---|
| `ghcr.io/keithmackay/modelrouter:latest` | SQLite only |
| `ghcr.io/keithmackay/modelrouter:latest-otel` | + OpenTelemetry |
| `ghcr.io/keithmackay/modelrouter:latest-postgres` | + PostgreSQL |
| `ghcr.io/keithmackay/modelrouter:latest-full` | All features |

docker pull ghcr.io/keithmackay/modelrouter:latest
docker run \
  -v /host/config:/config \
  -v /host/data:/data \
  -e MODELROUTER_CONFIG=/config/config.toml \
  -p 8080:8080 \
  ghcr.io/keithmackay/modelrouter:latest

# Port 8080 maps to server.port in config.toml (default 8080)
```

---

## Out of Scope

- Windows container images
- Helm chart updates (separate phase)
- Making the GHCR package public (deferred)
- Cross-compiling non-Linux targets into Docker
- `s3-archival` feature (unimplemented stub, excluded from all images)
