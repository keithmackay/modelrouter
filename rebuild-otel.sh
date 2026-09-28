#!/usr/bin/env bash
set -e

echo "==> Stopping and removing existing containers..."
docker compose -f docker-compose.otel.yml down --remove-orphans
docker rm -f $(docker ps -aq --filter "label=com.docker.compose.project=modelrouter") 2>/dev/null || true

echo "==> Removing old modelrouter image..."
docker rmi -f modelrouter:otel 2>/dev/null || true

echo "==> Building Rust binary inside Linux container (cached)..."
docker run --rm \
  --platform linux/arm64 \
  -v "$(pwd)":/workspace \
  -v modelrouter-cargo-registry:/usr/local/cargo/registry \
  -v modelrouter-cargo-git:/usr/local/cargo/git \
  -v modelrouter-target:/workspace/target \
  -w /workspace \
  rust:slim \
  sh -c "apt-get update -qq && apt-get install -y --no-install-recommends pkg-config libssl-dev && cargo build --release --features otel"

echo "==> Copying binary out of target volume..."
docker run --rm \
  -v modelrouter-target:/target \
  -v "$(pwd)/bin":/out \
  debian:trixie-slim \
  cp /target/release/modelrouter /out/modelrouter

echo "==> Building Docker image and starting stack..."
docker compose -f docker-compose.otel.yml up -d --build

echo "==> Done. Following logs (ctrl-c to detach)..."
docker compose -f docker-compose.otel.yml logs -f
