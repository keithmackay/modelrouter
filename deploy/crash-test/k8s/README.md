# Crash test on Kubernetes

The same test as `../run.sh`, on a real cluster: the Helm chart with
autoscaling on the in-flight gauge (Prometheus + prometheus-adapter), PostgreSQL,
Redis and the stub upstream in namespace `mr-crash`, load from a Job inside the
cluster, and one of: a pod's router process SIGKILLed (`kill`), a pod deleted
(`delete`, the path an eviction or scale-down takes), or no chaos.

```bash
# monitoring: Prometheus (scrapes the pods' prometheus.io annotations) and the adapter
helm install prom prometheus-community/prometheus -n monitoring --create-namespace -f prom-values.yaml
helm install adapter prometheus-community/prometheus-adapter -n monitoring -f adapter-values.yaml
# stores, stub, scripts
kubectl apply -f infra.yaml
kubectl -n mr-crash create configmap crash-scripts --from-file=../stub_upstream.py --from-file=../load.py
# an image built with --features postgres, pushed where the cluster can pull it
docker build --build-arg FEATURES=postgres -t localhost:5000/modelrouter:crash ../../.. && docker push localhost:5000/modelrouter:crash
helm install mr ../../helm/modelrouter -n mr-crash -f mr-values.yaml
# test-only: let a container SIGKILL the router without killing the pod
kubectl -n mr-crash patch deploy mr-modelrouter --type merge -p '{"spec":{"template":{"spec":{"shareProcessNamespace":true}}}}'
./run.sh NAME SECS WORKERS CHAOS_AT none|kill|kill2|delete
TARGET_URL=http://traefik.kube-system.svc ./run.sh ...   # through an ingress (retry-mw.yaml, ingress.yaml)
```

## Results (2026-11-22, k3s v1.37.2 single node, chart from #169/#176, HPA 2-6 pods at 6 in-flight per pod, 30% of requests streamed for 3 s)

| run | path | chaos | requests | failed | ok without a cost row |
|---|---|---|---|---|---|
| scale-up 2 → 6 pods, 40 workers | Service | none | 4689 | 1 (one stream timed out) | 0 |
| crash | Service | SIGKILL one pod's router at 40 s | 4034 | 25 (0.62%): 9 in flight on it, 16 connection refused before its endpoint was dropped | 0 |
| graceful delete | Service | pod deleted at 40 s | 3664 | 0 | 0 |
| scale-down 6 → 2 pods, 6 workers | Service | none | 1294 | 0 | 0 |
| crash | Traefik, retry middleware | SIGKILL one pod's router at 60 s | 4684 | 8 (0.17%): exactly the requests in flight on it | 0 |

Scaling in both directions, rollouts and evictions are invisible to callers.
A crash costs the requests that were in flight on the crashed pod; with an
ingress (or caller) that retries refused connections, nothing else.
