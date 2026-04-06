# Multi-stage build for modelrouter
#
# SQLite is bundled in the binary via sqlx's sqlite feature (which enables bundled libsqlite3).
# Config and database should be mounted as volumes at runtime:
#   -v /host/config:/config -v /host/data:/data
#
# Environment variables:
#   MODELROUTER_CONFIG=/config/config.toml
#   MODELROUTER_DATABASE__PATH=/data/router.db

# ── Builder stage ────────────────────────────────────────────────────────────
FROM rust:1.91-slim AS builder

WORKDIR /build

# Install build dependencies for SQLite bundled feature
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./

# Create a stub src/main.rs to pre-build dependencies
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs
RUN cargo build --release || true
RUN rm -rf src

# Copy full source and build for real
COPY . .
RUN cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /build/target/release/modelrouter /modelrouter

# Default command: start the HTTP server.
# Override with "migrate" to run database migrations.
CMD ["serve"]
ENTRYPOINT ["/modelrouter"]
