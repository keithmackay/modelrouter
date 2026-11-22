#!/usr/bin/env bash
# run.sh NAME SECS WORKERS CHAOS_AT CHAOS(none|kill|delete|kill2)
set -uo pipefail
export KUBECONFIG=~/.kube/config
N="-n mr-crash"; NAME=$1; SECS=$2; WORKERS=$3; AT=$4; CHAOS=$5
D=$(cd "$(dirname "$0")" && pwd)
pods() { kubectl $N get pod -l app.kubernetes.io/name=modelrouter --field-selector=status.phase=Running -o name; }
P=$(pods | head -1)
kubectl $N exec $P -c modelrouter -- /modelrouter user create --name $NAME >/dev/null 2>&1
KEY=$(kubectl $N exec $P -c modelrouter -- /modelrouter key create --user $NAME --project crash 2>&1 | grep -o 'mr-[A-Za-z0-9_-]*' | head -1)
sed -e "s|NAME|$NAME|g" -e "s|TARGET|${TARGET_URL:-http://mr-modelrouter:8080}|" -e "s|KEY|$KEY|" -e "s|SECS|$SECS|" -e "s|WORKERS|$WORKERS|" $D/load-job.tmpl | kubectl apply -f - >/dev/null
echo "[$(date +%T)] load $NAME started: $SECS s, $WORKERS workers"
start=$(date +%s)
( while [ $(( $(date +%s) - start )) -lt $((SECS + 20)) ]; do
    echo "[$(date +%T)] $(kubectl $N get hpa mr-modelrouter --no-headers | awk '{print $3,$4,"replicas="$7}') pods=$(kubectl $N get pod -l app.kubernetes.io/name=modelrouter --no-headers | awk '{print $3}' | sort | uniq -c | tr '\n' ' ')"
    sleep 10; done ) &
MON=$!
if [ "$CHAOS" != none ]; then
  sleep "$AT"
  V=$(pods | head -1)
  case $CHAOS in
    kill) echo "[$(date +%T)] SIGKILL router process in $V"
          kubectl $N exec $V -c modelrouter -- bash -c 'for p in /proc/[0-9]*; do grep -q "^/modelrouter" $p/cmdline 2>/dev/null && kill -9 ${p#/proc/}; done' ;;
    kill2) for V in $(pods | head -2); do echo "[$(date +%T)] SIGKILL router process in $V"
          kubectl $N exec $V -c modelrouter -- bash -c 'for p in /proc/[0-9]*; do grep -q "^/modelrouter" $p/cmdline 2>/dev/null && kill -9 ${p#/proc/}; done'; done ;;
    delete) echo "[$(date +%T)] delete (graceful) $V"; kubectl $N delete $V --wait=false ;;
  esac
fi
kubectl $N wait --for=condition=complete job/$NAME --timeout=$((SECS + 120))s >/dev/null 2>&1 || echo "job did not complete"
kill $MON 2>/dev/null
sleep 3
P=$(pods | head -1)
ROWS=$(kubectl $N exec $P -c modelrouter -- /modelrouter report cost --user $NAME --format json 2>/dev/null | python3 -c 'import json,sys; print(sum(r.get("request_count",0) for r in json.load(sys.stdin)))')
kubectl $N logs job/$NAME > $D/$NAME.json
python3 - $D/$NAME.json $ROWS <<'PY'
import json,sys
s=json.load(open(sys.argv[1])); rows=int(sys.argv[2])
print(f"RESULT requests {s['total']}, ok {s['ok']}, failed {s['failed']} ({100*s['failed']/max(1,s['total']):.2f}%)  plain {s['plain']}  stream {s['stream']}")
print(f"RESULT cost rows {rows} vs ok {s['ok']}: {'all present' if rows>=s['ok'] else str(s['ok']-rows)+' missing'}")
if s['failures']: print("first failures:", s['failures'][:8])
PY
