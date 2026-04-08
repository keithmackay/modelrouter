# Multi-stage build for modelrouter
#
# SQLite is bundled in the binary via sqlx's sqlite feature (which enables bundled libsqlite3).
# Config and database should be mounted as volumes at runtime:
#   -v /host/config:/config -v /host/data:/data
#
# Environment variables:
#   MODELROUTER_CONFIG=/config/config.toml
#   MODELROUTER_DATABASE__PATH=/data/router.db
#
# Build with vendored sources (no internet required):
#   cargo vendor && docker build ...
# Or with BuildKit registry cache (requires internet in Docker):
#   DOCKER_BUILDKIT=1 docker build ...

# ── Builder stage ────────────────────────────────────────────────────────────
FROM rust:1.91-slim AS builder

ARG FEATURES=""

WORKDIR /build

# Install build dependencies for SQLite bundled feature
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy everything (vendor/ and .cargo/config.toml used for offline builds when present)
COPY . .

RUN if [ -n "$FEATURES" ]; then \
      cargo build --release --features "$FEATURES"; \
    else \
      cargo build --release; \
    fi

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/modelrouter /modelrouter

ENTRYPOINT ["/modelrouter"]
# Default command: start the HTTP server.
# Override with "migrate" to run database migrations.
CMD ["serve"]
