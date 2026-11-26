#!/usr/bin/env bash
# Prove modelrouter on k3s: PostgreSQL + Redis + HPA, then scale-to-1 and
# startup time. No SQLite volume.
#
# The chart image is public Debian. A postgres-enabled binary on the node
# is bind-mounted with the host dynamic linker, because this environment
# cannot load a locally built image into k3s (no root, no local registry).
#
# Usage:
#   MODELROUTER_BIN=/path/to/modelrouter ./run.sh up
#   ./run.sh down
#
# up writes a timeline under /tmp/mr-autoscale-evidence and leaves the
# namespaces in place. down removes only mr-autoscale and mr-autoscale-mon.
#
# The chart refuses autoscaling.minReplicas below 2. The scale-to-1 phase
# patches the live HPA and does not change that guard.
set -uo pipefail

NS=mr-autoscale
MON=mr-autoscale-mon
REL=mr
DEP=mr-modelrouter
D=$(cd "$(dirname "$0")" && pwd)
EVID=${EVID:-/tmp/mr-autoscale-evidence}
TARGET=http://mr-modelrouter:8080
LOAD_SECS=${LOAD_SECS:-180}
LOAD_WORKERS=${LOAD_WORKERS:-32}
LIGHT_SECS=${LIGHT_SECS:-180}
LIGHT_WORKERS=${LIGHT_WORKERS:-4}
BURST_SECS=${BURST_SECS:-150}
BURST_WORKERS=${BURST_WORKERS:-32}
PROM_CHART_VERSION=${PROM_CHART_VERSION:-29.48.0}
ADAPTER_CHART_VERSION=${ADAPTER_CHART_VERSION:-5.4.2}

export PATH="${HOME}/.local/bin:${PATH}"
export KUBECONFIG="${KUBECONFIG:-${HOME}/.kube/config}"

die() { echo "ERROR: $*" >&2; exit 1; }

log() { echo "[$(date -Is)] $*" >&2; }

need() { command -v "$1" >/dev/null 2>&1 || die "missing $1"; }

replicas() {
  local r
  r=$(kubectl -n "$NS" get hpa "$DEP" -o jsonpath='{.status.currentReplicas}' 2>/dev/null || true)
  if [ -z "$r" ]; then
    r=$(kubectl -n "$NS" get deploy "$DEP" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)
  fi
  echo "${r:-0}"
}

ready_count() {
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=modelrouter -o json 2>/dev/null | python3 -c '
import json,sys
doc=json.load(sys.stdin)
n=0
for pod in doc.get("items",[]):
    if pod.get("metadata",{}).get("deletionTimestamp"):
        continue
    conds={c["type"]:c["status"] for c in pod.get("status",{}).get("conditions",[])}
    if conds.get("Ready")=="True":
        n+=1
print(n)
' 2>/dev/null || echo 0
}

ready_pod_name() {
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=modelrouter -o json | python3 -c '
import json,sys
doc=json.load(sys.stdin)
for pod in doc.get("items",[]):
    if pod.get("metadata",{}).get("deletionTimestamp"):
        continue
    conds={c["type"]:c["status"] for c in pod.get("status",{}).get("conditions",[])}
    if conds.get("Ready")=="True":
        print(pod["metadata"]["name"])
        break
'
}

mr() {
  local pod
  pod=$(ready_pod_name)
  [ -n "$pod" ] || return 1
  kubectl exec -n "$NS" "$pod" -c modelrouter -- \
    "$LINKER" "$BIN_PATH" "$@"
}

sql() {
  kubectl -n "$NS" exec deploy/postgres -c postgres -- \
    psql -U modelrouter -d modelrouter -tAc "$1"
}

snapshot() {
  local tag=$1
  local r
  r=$(replicas)
  log "SNAPSHOT $tag replicas=$r ready=$(ready_count)"
  kubectl -n "$NS" get hpa "$DEP" 2>/dev/null || true
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=modelrouter -o wide 2>/dev/null || true
  echo "$(date -Is),$r,$tag" >> "$EVID/replicas.csv"
}

dump_debug() {
  log "debug dump"
  kubectl -n "$NS" get pods -o wide || true
  kubectl -n "$NS" describe deploy "$DEP" | tail -60 || true
  kubectl -n "$NS" logs -l app.kubernetes.io/name=modelrouter -c migrate --tail=40 --prefix || true
  kubectl -n "$NS" logs -l app.kubernetes.io/name=modelrouter -c modelrouter --tail=40 --prefix || true
  local pod
  pod=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=modelrouter -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)
  if [ -n "$pod" ]; then
    kubectl -n "$NS" exec "$pod" -c modelrouter -- ls -l /host-usr-lib/ld-linux-x86-64.so.2 /opt/mr 2>&1 | head -20 || true
    kubectl -n "$NS" exec "$pod" -c modelrouter -- cat /etc/nsswitch.conf 2>&1 | head -20 || true
  fi
}

save_pod_timing() {
  local pod=$1 tag=$2
  mkdir -p "$EVID/startup"
  kubectl -n "$NS" get pod "$pod" -o json > "$EVID/startup/${tag}.pod.json"
  kubectl -n "$NS" get events --field-selector "involvedObject.name=${pod}" -o json \
    > "$EVID/startup/${tag}.events.json" 2>/dev/null || echo '{"items":[]}' > "$EVID/startup/${tag}.events.json"
  kubectl -n "$NS" describe pod "$pod" > "$EVID/startup/${tag}.describe.txt" 2>/dev/null || true
}

wait_ready() {
  local want=$1 timeout=$2
  local start now n
  start=$(date +%s)
  while true; do
    n=$(ready_count)
    now=$(date +%s)
    log "ready ${n}/${want} ($((now - start))s)"
    if [ "$n" -ge "$want" ]; then
      return 0
    fi
    if [ $((now - start)) -ge "$timeout" ]; then
      dump_debug
      return 1
    fi
    sleep 5
  done
}

wait_hpa_replicas() {
  local mode=$1 want=$2 timeout=$3
  local start now r
  start=$(date +%s)
  while true; do
    r=$(replicas)
    snapshot "wait-${mode}-${want}"
    now=$(date +%s)
    if [ "$mode" = ge ] && [ "$r" -ge "$want" ]; then return 0; fi
    if [ "$mode" = eq ] && [ "$r" -eq "$want" ]; then return 0; fi
    if [ $((now - start)) -ge "$timeout" ]; then
      kubectl -n "$NS" describe hpa "$DEP" || true
      return 1
    fi
    sleep 10
  done
}

patch_min() {
  local min=$1
  log "patch HPA minReplicas=${min} (chart will not render a value below 2)"
  kubectl -n "$NS" patch hpa "$DEP" --type merge -p "{\"spec\":{\"minReplicas\":${min}}}"
}

linker_paths() {
  [ -n "${MODELROUTER_BIN:-}" ] || die "set MODELROUTER_BIN to a postgres-enabled modelrouter binary on this node"
  [ -x "$MODELROUTER_BIN" ] || die "MODELROUTER_BIN is not executable: $MODELROUTER_BIN"
  BIN_DIR=$(dirname "$(readlink -f "$MODELROUTER_BIN")")
  BIN_NAME=$(basename "$(readlink -f "$MODELROUTER_BIN")")
  BIN_PATH="/opt/mr/${BIN_NAME}"
  if [ -e /usr/lib/ld-linux-x86-64.so.2 ]; then
    LINKER=/host-usr-lib/ld-linux-x86-64.so.2
  elif [ -e /lib64/ld-linux-x86-64.so.2 ]; then
    LINKER=/host-lib64/ld-linux-x86-64.so.2
  else
    die "host dynamic linker not found under /usr/lib or /lib64"
  fi
  LIBPATH=/host-usr-lib:/host-lib64:/host-usr-lib64
  log "binary ${BIN_DIR}/${BIN_NAME} linker ${LINKER}"
}

write_patch() {
  python3 - "$EVID/hostpath-patch.json" "$BIN_DIR" "$LINKER" "$BIN_PATH" "$LIBPATH" <<'PY'
import json, sys
path, bin_dir, linker, bin_path, libpath = sys.argv[1:]
mounts = [
    {"name": "host-bin", "mountPath": "/opt/mr", "readOnly": True},
    {"name": "host-lib64", "mountPath": "/host-lib64", "readOnly": True},
    {"name": "host-usr-lib", "mountPath": "/host-usr-lib", "readOnly": True},
    {"name": "host-usr-lib64", "mountPath": "/host-usr-lib64", "readOnly": True},
    {"name": "host-usr-bin", "mountPath": "/host-usr-bin", "readOnly": True},
    {"name": "nsswitch", "mountPath": "/etc/nsswitch.conf", "subPath": "nsswitch.conf", "readOnly": True},
]
env = [{"name": "LD_LIBRARY_PATH", "value": libpath}]
volumes = [
    {"name": "host-bin", "hostPath": {"path": bin_dir, "type": "Directory"}},
    {"name": "host-lib64", "hostPath": {"path": "/lib64", "type": "Directory"}},
    {"name": "host-usr-lib", "hostPath": {"path": "/usr/lib", "type": "Directory"}},
    {"name": "host-usr-lib64", "hostPath": {"path": "/usr/lib64", "type": "Directory"}},
    {"name": "host-usr-bin", "hostPath": {"path": "/usr/bin", "type": "Directory"}},
    {"name": "nsswitch", "configMap": {"name": "mr-nsswitch"}},
]
patch = {"spec": {"template": {"spec": {
    "initContainers": [{
        "name": "migrate",
        "command": [linker, bin_path, "migrate"],
        "env": env,
        "volumeMounts": mounts,
    }],
    "containers": [{
        "name": "modelrouter",
        "command": [linker, bin_path, "serve"],
        "env": env,
        "volumeMounts": mounts,
    }],
    "volumes": volumes,
}}}}
with open(path, "w") as fh:
    json.dump(patch, fh)
PY
}

# A previous proof left ClusterRoles owned by a helm release whose namespace
# is gone. Helm will not adopt them. Drop only those orphans.
drop_orphan_cluster_object() {
  local kind=$1 name=$2 where=${3:-} ns
  if [ -n "$where" ]; then
    ns=$(kubectl -n "$where" get "$kind" "$name" -o jsonpath='{.metadata.annotations.meta\.helm\.sh/release-namespace}' 2>/dev/null || true)
  else
    ns=$(kubectl get "$kind" "$name" -o jsonpath='{.metadata.annotations.meta\.helm\.sh/release-namespace}' 2>/dev/null || true)
  fi
  if [ -z "$ns" ]; then
    return 0
  fi
  if kubectl get ns "$ns" >/dev/null 2>&1; then
    return 0
  fi
  log "deleting orphan ${where:+$where/}${kind}/${name} (helm namespace ${ns} is gone)"
  if [ -n "$where" ]; then
    kubectl -n "$where" delete "$kind" "$name" --wait=false
  else
    kubectl delete "$kind" "$name" --wait=false
  fi
}

install_monitoring() {
  local kind name
  for kind in clusterrole clusterrolebinding; do
    for name in prom-prometheus-server \
      prometheus-adapter-resource-reader prometheus-adapter-server-resources \
      prometheus-adapter-hpa-controller prometheus-adapter-system-auth-delegator \
      prometheus-adapter-auth-reader; do
      drop_orphan_cluster_object "$kind" "$name"
    done
  done
  drop_orphan_cluster_object rolebinding prometheus-adapter-auth-reader kube-system
  log "helm prometheus ${PROM_CHART_VERSION} and adapter ${ADAPTER_CHART_VERSION}"
  helm repo update prometheus-community >/dev/null
  helm upgrade --install prom prometheus-community/prometheus \
    --version "$PROM_CHART_VERSION" -n "$MON" --create-namespace \
    -f "$D/prom-values.yaml" || die "prometheus install failed"
  helm upgrade --install adapter prometheus-community/prometheus-adapter \
    --version "$ADAPTER_CHART_VERSION" -n "$MON" \
    -f "$D/adapter-values.yaml" || die "prometheus-adapter install failed"
}

install_router() {
  linker_paths
  mkdir -p "$EVID"
  write_patch
  kubectl apply -f - <<EOF
apiVersion: v1
kind: ConfigMap
metadata: {name: mr-nsswitch, namespace: ${NS}}
data:
  nsswitch.conf: |
    passwd: files
    group: files
    shadow: files
    hosts: files dns
    networks: files
    protocols: files
    services: files
EOF
  helm upgrade --install "$REL" "$D/../helm/modelrouter" -n "$NS" -f "$D/mr-values.yaml"
  kubectl -n "$NS" patch deploy "$DEP" --type strategic --patch-file "$EVID/hostpath-patch.json"
  log "patched deployment with host binary and host linker"
}

wait_metric() {
  local start now raw
  start=$(date +%s)
  while true; do
    raw=$(kubectl get --raw "/apis/custom.metrics.k8s.io/v1beta1/namespaces/${NS}/pods/*/modelrouter_requests_in_flight" 2>/dev/null || true)
    if printf '%s' "$raw" | grep -q 'modelrouter_requests_in_flight'; then
      log "in-flight custom metric is being served"
      printf '%s\n' "$raw" > "$EVID/custom-metric.json"
      return 0
    fi
    now=$(date +%s)
    if [ $((now - start)) -ge 240 ]; then
      log "custom metric not ready after 240s"
      kubectl get --raw /apis/custom.metrics.k8s.io/v1beta1 2>&1 | head -c 2000 || true
      echo
      kubectl -n "$MON" get pods -o wide || true
      kubectl -n "$MON" logs -l app.kubernetes.io/name=prometheus-adapter --tail=40 || true
      return 1
    fi
    sleep 10
  done
}

create_key() {
  local name=$1 out key
  out=$(mr user create --name "$name" 2>&1) || true
  printf '%s\n' "$out" >> "$EVID/cli.txt"
  key=$(printf '%s\n' "$out" | grep -o 'mr-[A-Za-z0-9]*' | head -1 || true)
  if [ -z "$key" ]; then
    out=$(mr key create --user "$name" --project "proof-$(date +%s)" 2>&1) || true
    printf '%s\n' "$out" >> "$EVID/cli.txt"
    key=$(printf '%s\n' "$out" | grep -o 'mr-[A-Za-z0-9]*' | head -1 || true)
  fi
  [ -n "$key" ] || die "no API key for ${name}: ${out}"
  printf '%s' "$key"
}

ledger_count() {
  local name=$1
  sql "SELECT count(*) FROM cost_ledger c JOIN users u ON u.id = c.user_id WHERE u.name = '${name}'"
}

run_load() {
  local name=$1 secs=$2 workers=$3 stream=$4
  local key deadline
  key=$(create_key "$name")
  deadline=$((secs + 90))
  log "load ${name}: ${secs}s workers=${workers} stream=${stream}"
  sed -e "s|__NAME__|${name}|g" \
      -e "s|__TARGET__|${TARGET}|g" \
      -e "s|__KEY__|${key}|g" \
      -e "s|__SECS__|${secs}|g" \
      -e "s|__WORKERS__|${workers}|g" \
      -e "s|__STREAM__|${stream}|g" \
      -e "s|__DEADLINE__|${deadline}|g" \
      "$D/load-job.tmpl" | kubectl apply -f -
  echo "$(date -Is) LOAD_START ${name}" | tee -a "$EVID/phases.txt"
  local start now ok
  start=$(date +%s)
  while true; do
    snapshot "load-${name}"
    ok=$(kubectl -n "$NS" get job "$name" -o jsonpath='{.status.succeeded}' 2>/dev/null || true)
    if [ "${ok:-0}" = 1 ]; then
      break
    fi
    now=$(date +%s)
    if [ $((now - start)) -ge $((secs + 180)) ]; then
      kubectl -n "$NS" logs "job/${name}" > "$EVID/${name}.json" || true
      kubectl -n "$NS" describe "job/${name}" || true
      die "load job ${name} did not finish"
    fi
    sleep 5
  done
  kubectl -n "$NS" logs "job/${name}" > "$EVID/${name}.json"
  echo "$(date -Is) LOAD_END ${name}" | tee -a "$EVID/phases.txt"
  python3 - "$EVID/${name}.json" <<'PY'
import json,sys
doc=json.load(open(sys.argv[1]))
lat=doc.get("latency_ok_ms") or {}
print(f"RESULT {doc['run']} requests {doc['total']} ok {doc['ok']} failed {doc['failed']} latency_p50_ms {lat.get('p50_ms')} p95_ms {lat.get('p95_ms')} max_ms {lat.get('max_ms')}")
if doc.get("failures"):
    print("first failures:", doc["failures"][:8])
PY
}

wait_job_active() {
  local name=$1 i active
  for i in $(seq 1 40); do
    active=$(kubectl -n "$NS" get job "$name" -o jsonpath='{.status.active}' 2>/dev/null || true)
    if [ "${active:-0}" != 0 ] && [ "${active:-0}" != "" ]; then
      log "job ${name} active=${active}"
      return 0
    fi
    sleep 2
  done
  log "job ${name} did not become active"
  return 1
}

wait_ledger() {
  local name=$1 ok=$2
  local rows=0 i
  for i in 1 2 3 4 5 6 7 8; do
    rows=$(ledger_count "$name" | tr -d '[:space:]')
    log "ledger ${name} rows=${rows} ok=${ok}"
    if [ "${rows:-0}" -ge "$ok" ]; then
      echo "$rows"
      return 0
    fi
    sleep 5
  done
  echo "${rows:-0}"
  return 1
}

compare_ledger() {
  local name=$1
  local ok rows
  if [ ! -s "$EVID/${name}.json" ]; then
    log "no load result for ${name}"
    return 1
  fi
  ok=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["ok"])' "$EVID/${name}.json")
  failed=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["failed"])' "$EVID/${name}.json")
  if ! rows=$(wait_ledger "$name" "$ok"); then
    log "LEDGER SHORT ${name} rows=${rows} ok=${ok}"
    return 1
  fi
  log "LEDGER OK ${name} rows=${rows} client_ok=${ok} client_failed=${failed}"
  if [ "$failed" != 0 ]; then
    log "CLIENT FAILURES ${name} failed=${failed}"
    return 1
  fi
  return 0
}

sqlite_check() {
  local pod
  pod=$(ready_pod_name)
  log "sqlite check on ${pod}"
  # The image shell cannot run under the host libc, so use the host ls
  # through the host dynamic linker. LD_LIBRARY_PATH is for modelrouter.
  kubectl -n "$NS" exec "$pod" -c modelrouter -- \
    "$LINKER" --library-path "$LIBPATH" /host-usr-bin/ls -la \
    /root/.modelrouter /root/.modelrouter/router.db /tmp /data /proc/1/cwd \
    >"$EVID/sqlite-check.txt" 2>&1 || true
  cat "$EVID/sqlite-check.txt"
  kubectl -n "$NS" get pvc > "$EVID/pvc.txt" || true
  if grep -q 'router.db' "$EVID/sqlite-check.txt" && grep -q '^-' "$EVID/sqlite-check.txt"; then
    # ls prints a permission or missing-file line for every path. A real file
    # is a line that starts with '-' and contains router.db.
    if grep -E '^-' "$EVID/sqlite-check.txt" | grep -q 'router.db'; then
      log "SQLITE FILE PRESENT"
      return 1
    fi
  fi
  if grep -q 'No resources found' "$EVID/pvc.txt" || ! grep -q 'PersistentVolumeClaim\|pvc' "$EVID/pvc.txt"; then
    log "no PVC in ${NS}"
  fi
  if [ -s "$EVID/pvc.txt" ] && grep -q . "$EVID/pvc.txt" && ! grep -q 'No resources found' "$EVID/pvc.txt"; then
    log "PVC exists in ${NS}"
    cat "$EVID/pvc.txt"
    return 1
  fi
  log "no sqlite database file and no PVC"
  return 0
}

smoke() {
  local key
  key=$(create_key smoke)
  log "smoke chat completion"
  kubectl -n "$NS" exec deploy/stub -c stub -- python3 -c "
import json, urllib.request
req = urllib.request.Request('${TARGET}/v1/chat/completions',
    data=json.dumps({'model':'mock-model','messages':[{'role':'user','content':'smoke'}]}).encode(),
    headers={'authorization':'Bearer ${key}','content-type':'application/json'})
resp = urllib.request.urlopen(req, timeout=30)
body = resp.read()
assert resp.status == 200, resp.status
json.loads(body)
print('smoke-http', resp.status, len(body))
"
  local rows
  rows=$(wait_ledger smoke 1)
  log "smoke ledger rows=${rows}"
  [ "$rows" -ge 1 ] || return 1
}

startup_samples() {
  log "recording rollout pods, then 5 replacement startups"
  mkdir -p "$EVID/startup"
  sleep 2
  local pod tag i
  tag=0
  while read -r pod; do
    [ -n "$pod" ] || continue
    tag=$((tag + 1))
    save_pod_timing "$pod" "rollout-${tag}"
  done < <(kubectl -n "$NS" get pods -l app.kubernetes.io/name=modelrouter -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')

  local spec
  spec=$(kubectl -n "$NS" get deploy "$DEP" -o json)
  for i in 1 2 3 4 5; do
    printf '%s' "$spec" | python3 -c '
import json,sys
dep=json.load(sys.stdin)
tpl=dep["spec"]["template"]
meta=dict(tpl.get("metadata") or {})
labels=dict(meta.get("labels") or {})
for key in ("app.kubernetes.io/name","app.kubernetes.io/instance"):
    labels.pop(key, None)
labels["app"]="mr-startup"
meta["labels"]=labels
ann=dict(meta.get("annotations") or {})
for key in list(ann):
    if key.startswith("prometheus.io") or key.startswith("checksum/"):
        del ann[key]
meta["annotations"]=ann
meta["name"]=sys.argv[1]
meta["namespace"]=sys.argv[2]
spec=dict(tpl["spec"])
spec.pop("topologySpreadConstraints", None)
print(json.dumps({"apiVersion":"v1","kind":"Pod","metadata":meta,"spec":spec}))
' "mr-start-${i}" "$NS" | kubectl apply -f -
    kubectl -n "$NS" wait --for=condition=Ready "pod/mr-start-${i}" --timeout=180s \
      || { kubectl -n "$NS" describe "pod/mr-start-${i}"; kubectl -n "$NS" logs "pod/mr-start-${i}" -c migrate; die "startup sample ${i} not Ready"; }
    sleep 2
    save_pod_timing "mr-start-${i}" "sample-${i}"
    kubectl -n "$NS" delete pod "mr-start-${i}" --grace-period=0 --force --wait=false >/dev/null
  done
  python3 "$D/timing.py" "$EVID/startup" | tee "$EVID/startup-summary.txt"
}

pdb_note() {
  local pod port proxy i
  pod=$(ready_pod_name)
  log "PDB at $(ready_count) ready pod(s); attempting eviction of ${pod}"
  kubectl -n "$NS" get pdb -o yaml > "$EVID/pdb.yaml" || true
  kubectl -n "$NS" get pdb
  # Eviction is a pod subresource. This k3s kubectl rejects `kubectl create -f`
  # of kind Eviction ("no matches for kind Eviction"), so post through the proxy.
  port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
  kubectl proxy --port="$port" --accept-hosts='^127\.0\.0\.1$' >"$EVID/kubectl-proxy.log" 2>&1 &
  proxy=$!
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    curl -sf "http://127.0.0.1:${port}/readyz" >/dev/null && break
    sleep 0.2
  done
  curl -sS -D "$EVID/eviction.headers" -o "$EVID/eviction.body" -X POST \
    "http://127.0.0.1:${port}/api/v1/namespaces/${NS}/pods/${pod}/eviction" \
    -H 'Content-Type: application/json' \
    --data "{\"apiVersion\":\"policy/v1\",\"kind\":\"Eviction\",\"metadata\":{\"name\":\"${pod}\",\"namespace\":\"${NS}\"}}" \
    || true
  {
    echo "POST /api/v1/namespaces/${NS}/pods/${pod}/eviction"
    cat "$EVID/eviction.headers" 2>/dev/null || true
    echo
    cat "$EVID/eviction.body" 2>/dev/null || true
  } > "$EVID/eviction.txt"
  kill "$proxy" 2>/dev/null || true
  wait "$proxy" 2>/dev/null || true
  log "eviction result:"
  cat "$EVID/eviction.txt"
  log "drain: drainDelaySeconds=10 keeps the pod admitting after /health/ready goes 503; shutdown_grace_secs=90; terminationGracePeriodSeconds=120. At 1 replica there is no other pod to take traffic during that window. PDB minAvailable 1 refuses a voluntary eviction of the last pod (HTTP 429). HPA scale-down deletes the pod directly, so the budget does not stop a scale to 1. The chart also refuses minReplicas below 2."
}

capture_drain_logs() {
  kubectl -n "$NS" logs -l app.kubernetes.io/name=modelrouter -c modelrouter --tail=200 --prefix > "$EVID/drain-logs.txt" 2>/dev/null || true
  grep -E 'stopping:|draining|graceful shutdown' "$EVID/drain-logs.txt" | tail -40 || true
}

phase_scale() {
  local rc=0
  snapshot "before-heavy"
  run_load scale-up "$LOAD_SECS" "$LOAD_WORKERS" 0.3
  compare_ledger scale-up || rc=1
  local saw
  saw=$(awk -F, 'BEGIN{m=0} $2+0>m {m=$2+0} END{print m}' "$EVID/replicas.csv")
  log "peak replicas during run so far: ${saw}"
  if [ "${saw:-0}" -lt 4 ]; then
    log "did not reach 4 replicas"
    kubectl -n "$NS" describe hpa "$DEP" | tee "$EVID/hpa-miss.txt" || true
    rc=1
  else
    kubectl -n "$NS" describe hpa "$DEP" > "$EVID/hpa-scaled.txt" || true
  fi
  log "waiting for scale-down to 2 after the load"
  if ! wait_hpa_replicas eq 2 480; then
    log "did not return to 2 replicas"
    rc=1
  fi
  snapshot "after-scale-down-to-2"
  return "$rc"
}

phase_scale_to_one() {
  local rc=0 before after
  before=$(sql "SELECT count(*) FROM cost_ledger" | tr -d '[:space:]')
  log "idle scale-down to 1, cost_ledger before=${before}, no client load"
  # Follow logs so a drained pod's shutdown line is captured.
  kubectl -n "$NS" logs -l app.kubernetes.io/name=modelrouter -c modelrouter -f --prefix --tail=5 >"$EVID/idle-follow.txt" 2>&1 &
  local follow=$!
  patch_min 1
  if ! wait_hpa_replicas eq 1 300; then
    log "idle scale-down did not reach 1"
    rc=1
  fi
  sleep 8
  kill "$follow" 2>/dev/null || true
  wait "$follow" 2>/dev/null || true
  grep -E 'stopping:|draining|graceful shutdown' "$EVID/idle-follow.txt" | tee "$EVID/idle-drain-lines.txt" || true
  after=$(sql "SELECT count(*) FROM cost_ledger" | tr -d '[:space:]')
  log "idle scale-down cost_ledger after=${after} (before=${before})"
  if [ "$after" != "$before" ]; then
    log "ledger changed during idle scale-down"
    rc=1
  fi
  snapshot "at-1-idle"
  pdb_note || true
  # Back to 2 with no client traffic, then down again under light load.
  patch_min 2
  if ! wait_hpa_replicas eq 2 300; then
    log "did not return to 2 before light load"
    rc=1
  fi
  snapshot "back-to-2"
  log "light-load scale-down to 1"
  run_load scale-light "$LIGHT_SECS" "$LIGHT_WORKERS" 0 &
  local load_pid=$!
  wait_job_active scale-light || rc=1
  patch_min 1
  if ! wait_hpa_replicas eq 1 300; then
    log "light-load scale-down did not reach 1"
    rc=1
  fi
  snapshot "at-1-light"
  wait "$load_pid" || rc=1
  compare_ledger scale-light || rc=1
  capture_drain_logs || true
  log "burst from 1 pod"
  snapshot "before-burst"
  run_load scale-burst "$BURST_SECS" "$BURST_WORKERS" 0 &
  load_pid=$!
  wait_job_active scale-burst || rc=1
  if ! wait_hpa_replicas ge 2 240; then
    log "burst did not scale above 1"
    rc=1
  fi
  snapshot "burst-scaled"
  wait "$load_pid" || rc=1
  compare_ledger scale-burst || rc=1
  local peak
  peak=$(awk -F, 'BEGIN{m=0} $2+0>m {m=$2+0} END{print m}' "$EVID/replicas.csv")
  log "peak replicas including burst: ${peak}"
  return "$rc"
}

cmd_up() {
  need kubectl
  need helm
  need python3
  mkdir -p "$EVID/startup"
  : > "$EVID/replicas.csv"
  : > "$EVID/phases.txt"
  : > "$EVID/cli.txt"
  log "evidence ${EVID}"
  linker_paths
  kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -
  kubectl -n "$NS" create configmap crash-scripts \
    --from-file=stub_upstream.py="$D/../crash-test/stub_upstream.py" \
    --from-file=load.py="$D/load.py" \
    --dry-run=client -o yaml | kubectl apply -f -
  kubectl apply -f "$D/infra.yaml"
  kubectl -n "$NS" rollout status deploy/postgres --timeout=180s
  kubectl -n "$NS" rollout status deploy/redis --timeout=180s
  kubectl -n "$NS" rollout status deploy/stub --timeout=180s
  install_monitoring
  install_router
  kubectl -n "$MON" rollout status deploy/prom-prometheus-server --timeout=300s || kubectl -n "$MON" get pods -o wide
  kubectl -n "$MON" rollout status deploy/adapter-prometheus-adapter --timeout=300s || kubectl -n "$MON" get pods -o wide
  log "router command: $(kubectl -n "$NS" get deploy "$DEP" -o jsonpath='{.spec.template.spec.containers[0].command}')"
  if ! wait_ready 2 420; then
    die "router did not reach 2 ready pods"
  fi
  snapshot "baseline-2"
  smoke || { dump_debug; die "smoke request failed"; }
  sqlite_check || die "sqlite check failed after smoke"
  startup_samples || die "startup samples failed"
  local sample_rc=0
  local metric_rc=0
  if wait_metric; then
    echo in-flight > "$EVID/metric-mode.txt"
  else
    metric_rc=1
    echo cpu > "$EVID/metric-mode.txt"
    log "IN-FLIGHT METRIC UNAVAILABLE. Dropping it from the HPA so the CPU target can scale the pool."
    kubectl -n "$NS" patch hpa "$DEP" --type json -p '[{"op":"replace","path":"/spec/metrics","value":[{"type":"Resource","resource":{"name":"cpu","target":{"type":"Utilization","averageUtilization":40}}}]}]'
    kubectl -n "$NS" describe hpa "$DEP" | tee "$EVID/hpa-cpu-fallback.txt" || true
  fi
  local scale_rc=0 one_rc=0
  phase_scale || scale_rc=1
  phase_scale_to_one || one_rc=1
  sqlite_check || scale_rc=1
  log "postgres totals"
  sql "SELECT u.name, count(*) FROM cost_ledger c JOIN users u ON u.id = c.user_id GROUP BY 1 ORDER BY 1" | tee "$EVID/ledger-by-user.txt"
  sql "SELECT count(*) FROM cost_ledger" | tee "$EVID/ledger-total.txt"
  sql "SELECT count(*) FROM request_failures" | tee "$EVID/request-failures.txt" || true
  log "SUMMARY metric_rc=${metric_rc} sample_rc=${sample_rc} scale_rc=${scale_rc} one_rc=${one_rc}"
  log "startup summary follows"
  cat "$EVID/startup-summary.txt" 2>/dev/null || true
  if [ "$metric_rc$sample_rc$scale_rc$one_rc" != 0000 ]; then
    die "proof failed metric=${metric_rc} startup=${sample_rc} scale=${scale_rc} scale_to_1=${one_rc}"
  fi
  log "proof passed"
}

cmd_down() {
  log "removing proof releases and namespaces"
  helm uninstall adapter -n "$MON" 2>/dev/null || true
  helm uninstall prom -n "$MON" 2>/dev/null || true
  helm uninstall "$REL" -n "$NS" 2>/dev/null || true
  kubectl delete ns "$NS" "$MON" --wait=true --timeout=180s 2>/dev/null || true
  # The adapter's APIService is cluster-scoped. Uninstall should remove it.
  # If a failed install left it behind, drop only the one this proof created
  # when its service is already gone.
  if kubectl get apiservice v1beta1.custom.metrics.k8s.io >/dev/null 2>&1; then
    local svc
    svc=$(kubectl get apiservice v1beta1.custom.metrics.k8s.io -o jsonpath='{.spec.service.namespace}/{.spec.service.name}' 2>/dev/null || true)
    case "$svc" in
      "$MON"/*)
        log "removing leftover APIService v1beta1.custom.metrics.k8s.io (${svc})"
        kubectl delete apiservice v1beta1.custom.metrics.k8s.io --wait=false || true
        ;;
      *) log "leaving APIService v1beta1.custom.metrics.k8s.io (${svc})" ;;
    esac
  fi
  kubectl get ns
}

case "${1:-up}" in
  up) cmd_up ;;
  down) cmd_down ;;
  *) die "usage: $0 up|down" ;;
esac
