#!/usr/bin/env bash
# Render the Helm chart under the value combinations that matter for
# replicated and autoscaled deployments, and check what comes out.
# Needs helm 3 on PATH. Run from anywhere: deploy/helm/test-chart.sh
set -euo pipefail

CHART="$(cd "$(dirname "$0")/modelrouter" && pwd)"
fails=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; fails=$((fails + 1)); }

render() { helm template t "$CHART" "$@" 2>&1; }

# The one document of a kind from a multi-document render.
doc() { awk -v k="kind: $1" 'BEGIN{RS="\n---\n"} index($0, k) {print}'; }

expect_fail() { # name, expected message fragment, helm args...
  local name=$1 msg=$2 out
  shift 2
  if out=$(render "$@"); then
    fail "$name: rendered, expected a refusal"
  elif grep -qF -- "$msg" <<<"$out"; then
    pass "$name"
  else
    fail "$name: refused for the wrong reason: $out"
  fi
}

has() { # name, pattern, text
  if grep -qE -- "$2" <<<"$3"; then pass "$1"; else fail "$1: no match for /$2/"; fi
}
lacks() {
  if grep -qE -- "$2" <<<"$3"; then fail "$1: unexpected /$2/"; else pass "$1"; fi
}

if helm lint --strict "$CHART" >/dev/null; then pass "lint --strict (defaults)"; else fail "lint --strict (defaults)"; fi

# --- Defaults: one SQLite replica, fixed count, no HPA.
out=$(render)
dep=$(doc Deployment <<<"$out")
has   "default: fixed replicas" '^  replicas: 1$' "$dep"
has   "default: SQLite path on the volume" 'MODELROUTER_DATABASE__PATH' "$dep"
lacks "default: no HPA" 'kind: HorizontalPodAutoscaler' "$out"
lacks "default: no postgres env" 'POSTGRES_URL' "$dep"
has   "default: scrape annotation" 'prometheus.io/scrape: "true"' "$dep"
has   "default: drain delay" 'MODELROUTER_SERVER__DRAIN_DELAY_SECS[[:space:]]+value: "10"' "$(tr '\n' ' ' <<<"$dep")"

# --- Autoscaling refusals.
AS=(--set autoscaling.enabled=true)
PG=(--set persistence.enabled=false --set database.postgresUrl=postgres://u:p@db:5432/mr)
REDIS=(--set cluster.backend=redis --set cluster.redisUrl=redis://redis:6379)
expect_fail "autoscaling refuses the memory backend" "requires cluster.backend: redis" "${AS[@]}" "${PG[@]}"
expect_fail "autoscaling refuses the SQLite volume" "requires persistence.enabled: false" "${AS[@]}" "${REDIS[@]}" --set database.postgresUrl=postgres://x
expect_fail "autoscaling refuses no PostgreSQL" "requires a PostgreSQL store" "${AS[@]}" "${REDIS[@]}" --set persistence.enabled=false
expect_fail "autoscaling refuses minReplicas 1" "minReplicas must be at least 2" "${AS[@]}" "${REDIS[@]}" "${PG[@]}" --set autoscaling.minReplicas=1
expect_fail "autoscaling refuses max < min" "maxReplicas must be at least" "${AS[@]}" "${REDIS[@]}" "${PG[@]}" --set autoscaling.minReplicas=4 --set autoscaling.maxReplicas=3
expect_fail "autoscaling refuses no target" "needs at least one target" "${AS[@]}" "${REDIS[@]}" "${PG[@]}" --set autoscaling.targetCPUUtilizationPercentage=

# --- Autoscaling, valid.
out=$(render "${AS[@]}" "${REDIS[@]}" "${PG[@]}" --set autoscaling.targetInFlightPerPod=20 --set autoscaling.maxReplicas=8)
dep=$(doc Deployment <<<"$out")
hpa=$(doc HorizontalPodAutoscaler <<<"$out")
lacks "autoscaling: Deployment leaves replicas to the HPA" '^  replicas:' "$dep"
has   "autoscaling: fallback share sized by maxReplicas" 'MODELROUTER_CLUSTER__REPLICAS[[:space:]]+value: "8"' "$(tr '\n' ' ' <<<"$dep")"
lacks "autoscaling: no SQLite path" 'MODELROUTER_DATABASE__PATH' "$dep"
lacks "autoscaling: no PVC" 'kind: PersistentVolumeClaim' "$out"
has   "autoscaling: postgres url from the chart secret" 'key: postgres-url' "$dep"
has   "autoscaling: secret carries the url" 'postgres-url: "postgres://u:p@db:5432/mr"' "$(doc Secret <<<"$out")"
has   "autoscaling: redis backend" 'value: "redis"' "$dep"
has   "hpa: in-flight Pods metric" 'name: modelrouter_requests_in_flight' "$hpa"
has   "hpa: in-flight target" 'averageValue: "20"' "$hpa"
has   "hpa: CPU fallback" 'name: cpu' "$hpa"
lacks "hpa: memory off by default" 'name: memory' "$hpa"
has   "hpa: slow scale-down" 'stabilizationWindowSeconds: 600' "$hpa"
has   "hpa: bounds" 'minReplicas: 2' "$hpa"
n=$(grep -c 'MODELROUTER_DATABASE__POSTGRES_URL' <<<"$dep" || true)
if [ "$n" = 2 ]; then pass "autoscaling: migrate init container and router both get the url"; else fail "postgres url env count $n, want 2"; fi
if helm lint --strict "$CHART" "${AS[@]}" "${REDIS[@]}" "${PG[@]}" >/dev/null; then pass "lint --strict (autoscaling)"; else fail "lint --strict (autoscaling)"; fi

# --- An existing secret instead of a URL in values; postgres_url in config also counts.
out=$(render "${AS[@]}" "${REDIS[@]}" --set persistence.enabled=false --set database.existingSecret=pg --set database.existingSecretKey=url)
dep=$(doc Deployment <<<"$out")
has   "existingSecret: referenced" 'name: pg$' "$dep"
lacks "existingSecret: not copied into the chart secret" 'postgres-url:' "$(doc Secret <<<"$out")"
cfg=$'[database]\npostgres_url = "postgres://in-config"\n'
if render "${AS[@]}" "${REDIS[@]}" --set persistence.enabled=false --set-string config="$cfg" >/dev/null; then
  pass "postgres_url in config satisfies the store check"
else
  fail "postgres_url in config not recognised"
fi

echo
if [ "$fails" -gt 0 ]; then echo "$fails check(s) failed"; exit 1; fi
echo "all chart checks passed"
