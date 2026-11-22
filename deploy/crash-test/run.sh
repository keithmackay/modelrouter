#!/usr/bin/env bash
# Local crash test: several `modelrouter serve` replicas on one PostgreSQL and
# one Redis, behind a load balancer, under steady load, while one replica is
# killed (SIGKILL, then replaced) or drained (SIGTERM, as in a rollout).
# Counts what callers saw and whether every successful request left a cost
# row. No containers or root needed. See README.md beside this script.
#
#   MODELROUTER_BIN=target/release/modelrouter \
#   CRASH_PG_URL=postgres://postgres@127.0.0.1:55432/postgres \
#   CRASH_REDIS_URL=redis://127.0.0.1:6379 \
#   deploy/crash-test/run.sh [kill|drain] [retry|noretry] [secs]
set -euo pipefail

SCENARIO=${1:-kill}
MODE=${2:-retry}
SECS=${3:-45}
REPLICAS=${CRASH_REPLICAS:-3}
WORKERS=${CRASH_WORKERS:-16}
# [server] drain_delay_secs for every replica. The balancer drops a backend
# after 3 failed 1 s probes, so 5 covers it. Needs a build that knows the key.
BIN=${MODELROUTER_BIN:?set MODELROUTER_BIN to a binary built with --features postgres}
PG=${CRASH_PG_URL:?set CRASH_PG_URL to a scratch PostgreSQL database}
REDIS=${CRASH_REDIS_URL:?set CRASH_REDIS_URL to a scratch Redis}
HERE=$(cd "$(dirname "$0")" && pwd)
BASE_PORT=${CRASH_BASE_PORT:-18080}
LB_PORT=$BASE_PORT
STUB_PORT=$((BASE_PORT + 50))
RUN="crash-$SCENARIO-$MODE-$(date +%s)"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/mr-crash.XXXXXX")
PIDS=()

cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for i in $(seq 1 "$REPLICAS"); do
    [ -f "$WORK/r$i.pid" ] && kill -9 "$(cat "$WORK/r$i.pid")" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT

log() { echo "[$(date +%T)] $*"; }

config() { # port
  cat <<EOF
[server]
host = "127.0.0.1"
port = $1
shutdown_grace_secs = 30
drain_delay_secs = ${CRASH_DRAIN_DELAY:-0}

[database]
path = "$WORK/unused.db"
postgres_url = "$PG"

[routing]
default_provider = "mock"
default_model = "mock-model"

[providers.mock]
api_base = "http://127.0.0.1:$STUB_PORT/v1"
api_key = "stub-key-not-a-secret"

[auth]
jwt_secret = "crash-test-secret-crash-test-secret-0123456789"

[cache]
enabled = false

[cluster]
replicas = $REPLICAS
backend = "redis"
redis_url = "$REDIS"
namespace = "$RUN"

[[pricing]]
model = "mock-model"
input_per_million = 1.0
output_per_million = 2.0
EOF
}

start_replica() { # index
  local i=$1 port=$((BASE_PORT + $1))
  config "$port" >"$WORK/r$i.toml"
  MODELROUTER_CONFIG="$WORK/r$i.toml" RUST_LOG=${CRASH_LOG:-error} "$BIN" serve >>"$WORK/r$i.log" 2>&1 &
  echo $! >"$WORK/r$i.pid"
  for _ in $(seq 1 100); do
    curl -sf "http://127.0.0.1:$port/health/ready" >/dev/null && return 0
    sleep 0.1
  done
  log "replica $i did not become ready"; tail -20 "$WORK/r$i.log"; return 1
}

log "work dir $WORK, run $RUN"
python3 "$HERE/stub_upstream.py" "$STUB_PORT" & PIDS+=($!)
for i in $(seq 1 "$REPLICAS"); do start_replica "$i"; done
log "$REPLICAS replicas ready"

USER_NAME="$RUN"
MODELROUTER_CONFIG="$WORK/r1.toml" "$BIN" user create --name "$USER_NAME" >/dev/null
KEY=$(MODELROUTER_CONFIG="$WORK/r1.toml" "$BIN" key create --user "$USER_NAME" --project crash \
  | tr -s ' \t' '\n' | grep -m1 '^mr-' | tr -cd 'A-Za-z0-9-')

BACKENDS=()
for i in $(seq 1 "$REPLICAS"); do BACKENDS+=(--backend "127.0.0.1:$((BASE_PORT + i))"); done
RETRY=(); [ "$MODE" = retry ] && RETRY=(--retry)
python3 "$HERE/lb.py" --listen "$LB_PORT" "${BACKENDS[@]}" "${RETRY[@]}" >"$WORK/lb.log" 2>&1 & PIDS+=($!)
sleep 2.5  # the balancer's first readiness round

python3 "$HERE/load.py" --url "http://127.0.0.1:$LB_PORT" --key "$KEY" --secs "$SECS" \
  --workers "$WORKERS" --run "$RUN" --ok-ids "$WORK/ok.txt" >"$WORK/summary.json" &
LOAD=$!

sleep "$(python3 -c "print($SECS/3)")"
VICTIM=1
VPID=$(cat "$WORK/r$VICTIM.pid")
if [ "$SCENARIO" = kill ]; then
  log "SIGKILL replica $VICTIM (pid $VPID)"
  kill -9 "$VPID"
  sleep 5  # the Deployment notices and schedules a replacement
else
  log "SIGTERM replica $VICTIM (pid $VPID): drain"
  kill -TERM "$VPID"
  while kill -0 "$VPID" 2>/dev/null; do sleep 0.2; done
  log "replica $VICTIM exited after draining"
fi
log "replacing replica $VICTIM"
start_replica "$VICTIM"
wait "$LOAD"

sleep 3  # let spawned ledger writes land
ROWS=$(MODELROUTER_CONFIG="$WORK/r2.toml" "$BIN" report cost --user "$USER_NAME" --format json 2>/dev/null \
  | python3 -c '
import json,sys
d=json.load(sys.stdin)
rows=d if isinstance(d,list) else d.get("rows", d.get("data", [d]))
n=0
for r in rows:
    for k in ("request_count","requests","count","calls"):
        if k in r: n+=int(r[k]); break
print(n)')
OK=$(wc -l <"$WORK/ok.txt" | tr -d ' ')

echo
log "scenario=$SCENARIO mode=$MODE secs=$SECS replicas=$REPLICAS workers=$WORKERS drain_delay=${CRASH_DRAIN_DELAY:-0}"
python3 - "$WORK/summary.json" "$OK" "$ROWS" "$(grep -o 'retries so far [0-9]*' "$WORK/lb.log" | tail -1 | grep -o '[0-9]*$' || echo 0)" <<'EOF'
import json, sys
s = json.load(open(sys.argv[1])); ok, rows, retries = int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
print(f"requests {s['total']}, ok {s['ok']}, failed {s['failed']} ({100*s['failed']/max(1,s['total']):.2f}%)")
print(f"  plain:  {s['plain']}")
print(f"  stream: {s['stream']}")
print(f"balancer retries: {retries}")
print(f"cost rows for the run's user: {rows} (successful requests {ok}; {'every success has a row' if rows >= ok else f'{ok-rows} missing'})")
if s["failures"]:
    print("first failures (t, kind, outcome):", s["failures"][:10])
EOF
echo "logs: $WORK"
