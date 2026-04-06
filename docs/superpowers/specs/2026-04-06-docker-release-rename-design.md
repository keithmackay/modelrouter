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

GitHub automatically redirects old URLs after a rename, so existing clones continue to work.

### Local folder renames (done in shell after GitHub rename)

```bash
mv ~/Projects/modelrouter ~/Projects/modelrouter_py
mv ~/Projects/tokenomics ~/Projects/modelrouter
```

### Internal reference updates (in this repo)

Six files reference `tokenomics` and must be updated to `modelrouter`:

| File | Change |
|---|---|
| `README.md` | Clone URL, badge URL, install instructions |
| `CONTRIBUTING.md` | Repo URL reference |
| `docs/local-setup.md` | Clone URL or path references |
| `deploy/helm/modelrouter/Chart.yaml` | Source URL or annotation |
| `docs/superpowers/plans/2026-04-05-phase18-declarative-policy-engine.md` | Repo reference |
| `docs/superpowers/plans/2026-04-05-phase16-helm-charts.md` | Repo reference |

### README.md additions (Docker section)

The existing Docker section shows only a local `docker build`. It will be expanded to include:

- GHCR pull instructions for each variant
- Variant table (default, otel, postgres, full) with feature descriptions
- Volume mount and environment variable reference

---

## Phase 2 — Docker Distribution

### Image variants

| Tag suffix | Cargo features | Use case |
|---|---|---|
| _(none)_ | `[]` | SQLite only, minimal footprint |
| `-otel` | `otel` | + OpenTelemetry traces/metrics/logs |
| `-postgres` | `postgres` | + PostgreSQL backend |
| `-full` | `otel,postgres,bedrock,prometheus` | All optional features |

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

### Architecture

Each image tag is a multi-arch manifest covering:
- `linux/amd64` (x86_64)
- `linux/arm64` (aarch64)

Built with `docker buildx` + QEMU emulation on GitHub-hosted runners.

### Dockerfile changes

The existing `Dockerfile` is extended with a `FEATURES` build argument:

```dockerfile
ARG FEATURES=""
# ...
RUN cargo build --release \
    $([ -n "$FEATURES" ] && echo "--features $FEATURES" || true)
```

The stub pre-build step also passes `FEATURES` so the dependency cache remains valid across variants.

### GitHub Actions workflow changes

A new `docker` job is added to `.github/workflows/release.yml`, running after the existing `build` job completes. It:

1. Checks out the repo
2. Logs in to GHCR using `GITHUB_TOKEN`
3. Sets up QEMU and `docker buildx`
4. Runs a matrix of 4 variants, building and pushing each multi-arch image
5. Sets package visibility to private via the GHCR API (or relies on repo default)

```yaml
docker:
  name: Docker ${{ matrix.variant }}
  needs: build
  runs-on: ubuntu-latest
  permissions:
    contents: read
    packages: write
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
        tags: |
          ghcr.io/${{ github.repository_owner }}/modelrouter:${{ github.ref_name }}${{ matrix.tag_suffix }}
          ghcr.io/${{ github.repository_owner }}/modelrouter:latest${{ matrix.tag_suffix }}
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

```bash
docker pull ghcr.io/keithmackay/modelrouter:latest
docker run \
  -v /host/config:/config \
  -v /host/data:/data \
  -e MODELROUTER_CONFIG=/config/config.toml \
  -p 8080:8080 \
  ghcr.io/keithmackay/modelrouter:latest
```
```

---

## Out of Scope

- Windows container images
- Helm chart updates (separate phase)
- Making the GHCR package public (deferred)
- Cross-compiling non-Linux targets into Docker
