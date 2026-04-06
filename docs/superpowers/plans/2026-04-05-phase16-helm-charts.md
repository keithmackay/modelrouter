# Phase 16: Kubernetes / Helm Charts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a production-ready Helm chart and a corrected Dockerfile that lets operators deploy modelrouter on Kubernetes with one `helm install` command.

**Architecture:** A Helm chart lives at `deploy/helm/modelrouter/`. The chart creates a Deployment (with an init container that runs `modelrouter migrate` before the server starts), a Service, a PersistentVolumeClaim for the SQLite database, a ConfigMap for `config.toml`, a Secret for provider API keys, and an optional HPA. The existing Dockerfile is updated with a `CMD ["serve"]` default and a corrected Rust version. Sensitive values (API keys, JWT secret) are exposed as environment variables so they can be injected from the Secret without embedding them in the ConfigMap. The health endpoint already returns `200 OK` and is probe-compatible as-is.

**Tech Stack:** Helm 3, Kubernetes 1.26+, Docker multi-stage build (existing), YAML

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `Dockerfile` | Modify | Add `CMD ["serve"]`, fix Rust version (1.75 → 1.91) |
| `deploy/helm/modelrouter/Chart.yaml` | Create | Chart metadata |
| `deploy/helm/modelrouter/values.yaml` | Create | All tuneable defaults |
| `deploy/helm/modelrouter/templates/_helpers.tpl` | Create | `modelrouter.fullname`, `modelrouter.labels`, `modelrouter.selectorLabels` helpers |
| `deploy/helm/modelrouter/templates/configmap.yaml` | Create | Mounts `config.toml` from `values.config` |
| `deploy/helm/modelrouter/templates/secret.yaml` | Create | Provider API keys + JWT secret |
| `deploy/helm/modelrouter/templates/pvc.yaml` | Create | SQLite data volume |
| `deploy/helm/modelrouter/templates/deployment.yaml` | Create | Main workload with init container + probes |
| `deploy/helm/modelrouter/templates/service.yaml` | Create | ClusterIP Service |
| `deploy/helm/modelrouter/templates/hpa.yaml` | Create | HPA (disabled by default, enabled via `values.autoscaling.enabled`) |

---

### Task 1: Fix Dockerfile

**Files:**
- Modify: `Dockerfile`

The existing Dockerfile has two problems:
1. Uses `rust:1.75-slim` but `Cargo.toml` requires `rust-version = "1.91"` — this will silently fail for features requiring 1.91.
2. No `CMD` instruction — running the container with no arguments runs the binary with no subcommand, which prints help instead of serving.

- [ ] **Step 1: Verify current Dockerfile compiles (no-op if CI already passes)**

Run: `cargo build --release 2>&1 | tail -5`
Expected: success

- [ ] **Step 2: Update `Dockerfile`**

Replace the builder stage `FROM` line and add `CMD`:

```dockerfile
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
```

- [ ] **Step 3: Verify the Dockerfile change is syntactically valid**

Run: `docker build --no-cache --progress=plain -t modelrouter:dev . 2>&1 | tail -10`
Expected: `Successfully built ...` (or equivalent BuildKit output)

If docker build is unavailable or too slow, verify via: `grep -n "^FROM\|^CMD\|^ENTRYPOINT" Dockerfile`
Expected output:
```
1:FROM rust:1.91-slim AS builder
29:FROM gcr.io/distroless/cc-debian12
33:CMD ["serve"]
34:ENTRYPOINT ["/modelrouter"]
```

- [ ] **Step 4: Run cargo tests to verify nothing regressed**

Run: `cargo test 2>&1 | grep "test result" | tail -5`
Expected: all ok

- [ ] **Step 5: Commit**

```bash
git add Dockerfile
git commit -m "fix: update Dockerfile rust version to 1.91 and add CMD serve default"
```

---

### Task 2: Helm chart scaffolding (Chart.yaml, values.yaml, _helpers.tpl)

**Files:**
- Create: `deploy/helm/modelrouter/Chart.yaml`
- Create: `deploy/helm/modelrouter/values.yaml`
- Create: `deploy/helm/modelrouter/templates/_helpers.tpl`

- [ ] **Step 1: Create directory structure**

```bash
mkdir -p deploy/helm/modelrouter/templates
```

- [ ] **Step 2: Create `deploy/helm/modelrouter/Chart.yaml`**

```yaml
apiVersion: v2
name: modelrouter
description: LLM proxy with budget enforcement, routing, and guardrails
type: application
version: 0.1.0
appVersion: "0.1.0"
keywords:
  - llm
  - ai
  - proxy
home: https://github.com/keithmackay/tokenomics
```

- [ ] **Step 3: Create `deploy/helm/modelrouter/values.yaml`**

```yaml
# Number of replicas. Keep at 1 when using SQLite (single-writer constraint).
replicaCount: 1

image:
  repository: ghcr.io/keithmackay/modelrouter
  tag: "latest"
  pullPolicy: IfNotPresent

# Service configuration
service:
  type: ClusterIP
  port: 8080

# Resource requests and limits
resources:
  requests:
    cpu: 100m
    memory: 128Mi
  limits:
    cpu: 500m
    memory: 512Mi

# Persistent storage for SQLite database
persistence:
  enabled: true
  storageClass: ""     # "" uses cluster default
  size: 1Gi
  accessMode: ReadWriteOnce

# Horizontal Pod Autoscaler — disable when using SQLite (single-writer)
autoscaling:
  enabled: false
  minReplicas: 1
  maxReplicas: 3
  targetCPUUtilizationPercentage: 70
  targetMemoryUtilizationPercentage: 80

# config.toml content rendered into a ConfigMap.
# Sensitive values (api_key, jwt_secret) should be provided via `secrets` below
# and will be injected as environment variables, not embedded in the config file.
config: |
  [server]
  host = "0.0.0.0"
  port = 8080

  [database]
  path = "/data/router.db"

  [routing]
  default_provider = "openai"
  default_model = "gpt-4o"

  [auth]
  jwt_secret = ""  # overridden by MODELROUTER__AUTH__JWT_SECRET env var

# Provider API keys and other secrets injected as environment variables.
# These become env vars of the form MODELROUTER__PROVIDERS__<UPPER>__API_KEY.
secrets:
  jwtSecret: "change-me"
  providers: {}
  # providers:
  #   openai: "sk-..."
  #   anthropic: "sk-ant-..."

# Node selector, tolerations, affinity
nodeSelector: {}
tolerations: []
affinity: {}
```

- [ ] **Step 4: Create `deploy/helm/modelrouter/templates/_helpers.tpl`**

```
{{/*
Expand the name of the chart.
*/}}
{{- define "modelrouter.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "modelrouter.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "modelrouter.labels" -}}
helm.sh/chart: {{ include "modelrouter.chart" . }}
{{ include "modelrouter.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "modelrouter.selectorLabels" -}}
app.kubernetes.io/name: {{ include "modelrouter.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Chart name and version for the label
*/}}
{{- define "modelrouter.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}
```

- [ ] **Step 5: Run `helm lint` to verify scaffolding is valid**

Run: `helm lint deploy/helm/modelrouter/`
Expected: `==> Linting deploy/helm/modelrouter/` followed by `1 chart(s) linted, 0 chart(s) failed`

Note: lint may warn about missing templates — that is expected at this stage.

- [ ] **Step 6: Commit**

```bash
git add deploy/helm/modelrouter/
git commit -m "feat: helm chart scaffolding — Chart.yaml, values.yaml, helpers"
```

---

### Task 3: ConfigMap, Secret, PVC, and Service templates

**Files:**
- Create: `deploy/helm/modelrouter/templates/configmap.yaml`
- Create: `deploy/helm/modelrouter/templates/secret.yaml`
- Create: `deploy/helm/modelrouter/templates/pvc.yaml`
- Create: `deploy/helm/modelrouter/templates/service.yaml`

- [ ] **Step 1: Create `deploy/helm/modelrouter/templates/configmap.yaml`**

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "modelrouter.fullname" . }}-config
  labels:
    {{- include "modelrouter.labels" . | nindent 4 }}
data:
  config.toml: |
{{ .Values.config | indent 4 }}
```

Note: `indent 4` (not `nindent`) is the correct pattern here — it adds 4 spaces to each line without prepending a newline, keeping the YAML literal block scalar well-formed.

- [ ] **Step 2: Create `deploy/helm/modelrouter/templates/secret.yaml`**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: {{ include "modelrouter.fullname" . }}-secrets
  labels:
    {{- include "modelrouter.labels" . | nindent 4 }}
type: Opaque
stringData:
  jwt-secret: {{ .Values.secrets.jwtSecret | quote }}
  {{- range $provider, $key := .Values.secrets.providers }}
  provider-{{ $provider }}: {{ $key | quote }}
  {{- end }}
```

- [ ] **Step 3: Create `deploy/helm/modelrouter/templates/pvc.yaml`**

```yaml
{{- if .Values.persistence.enabled }}
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ include "modelrouter.fullname" . }}-data
  labels:
    {{- include "modelrouter.labels" . | nindent 4 }}
spec:
  accessModes:
    - {{ .Values.persistence.accessMode }}
  resources:
    requests:
      storage: {{ .Values.persistence.size }}
  {{- if .Values.persistence.storageClass }}
  storageClassName: {{ .Values.persistence.storageClass }}
  {{- end }}
{{- end }}
```

- [ ] **Step 4: Create `deploy/helm/modelrouter/templates/service.yaml`**

```yaml
apiVersion: v1
kind: Service
metadata:
  name: {{ include "modelrouter.fullname" . }}
  labels:
    {{- include "modelrouter.labels" . | nindent 4 }}
spec:
  type: {{ .Values.service.type }}
  ports:
    - port: {{ .Values.service.port }}
      targetPort: http
      protocol: TCP
      name: http
  selector:
    {{- include "modelrouter.selectorLabels" . | nindent 4 }}
```

- [ ] **Step 5: Run `helm lint` and `helm template` to verify**

Run: `helm lint deploy/helm/modelrouter/`
Expected: `1 chart(s) linted, 0 chart(s) failed`

Run: `helm template test-release deploy/helm/modelrouter/ 2>&1 | head -40`
Expected: YAML output with ConfigMap, Secret, PVC, and Service resources

- [ ] **Step 6: Commit**

```bash
git add deploy/helm/modelrouter/templates/configmap.yaml \
        deploy/helm/modelrouter/templates/secret.yaml \
        deploy/helm/modelrouter/templates/pvc.yaml \
        deploy/helm/modelrouter/templates/service.yaml
git commit -m "feat: helm chart ConfigMap, Secret, PVC, and Service templates"
```

---

### Task 4: Deployment and HPA templates

**Files:**
- Create: `deploy/helm/modelrouter/templates/deployment.yaml`
- Create: `deploy/helm/modelrouter/templates/hpa.yaml`

The Deployment has two key features:
1. **Init container** — runs `modelrouter migrate` to apply DB migrations before the server starts. It mounts the same PVC as the main container so the migration writes to the correct database file.
2. **HTTP probes** — liveness and readiness probes hit `GET /health`, which already returns `200 OK`.

- [ ] **Step 1: Create `deploy/helm/modelrouter/templates/deployment.yaml`**

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "modelrouter.fullname" . }}
  labels:
    {{- include "modelrouter.labels" . | nindent 4 }}
spec:
  replicas: {{ .Values.replicaCount }}
  selector:
    matchLabels:
      {{- include "modelrouter.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "modelrouter.selectorLabels" . | nindent 8 }}
      annotations:
        # Force pod restart when config changes
        checksum/config: {{ include (print $.Template.BasePath "/configmap.yaml") . | sha256sum }}
        checksum/secret: {{ include (print $.Template.BasePath "/secret.yaml") . | sha256sum }}
    spec:
      {{- with .Values.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      initContainers:
        - name: migrate
          image: "{{ .Values.image.repository }}:{{ .Values.image.tag }}"
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          command: ["/modelrouter", "migrate"]
          env:
            - name: MODELROUTER_CONFIG
              value: /config/config.toml
            - name: MODELROUTER__DATABASE__PATH
              value: /data/router.db
          volumeMounts:
            - name: config
              mountPath: /config
              readOnly: true
            {{- if .Values.persistence.enabled }}
            - name: data
              mountPath: /data
            {{- end }}
      containers:
        - name: modelrouter
          image: "{{ .Values.image.repository }}:{{ .Values.image.tag }}"
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          ports:
            - name: http
              containerPort: 8080
              protocol: TCP
          env:
            - name: MODELROUTER_CONFIG
              value: /config/config.toml
            - name: MODELROUTER__DATABASE__PATH
              value: /data/router.db
            - name: MODELROUTER__AUTH__JWT_SECRET
              valueFrom:
                secretKeyRef:
                  name: {{ include "modelrouter.fullname" . }}-secrets
                  key: jwt-secret
            {{- range $provider, $_ := .Values.secrets.providers }}
            - name: MODELROUTER__PROVIDERS__{{ $provider | upper }}__API_KEY
              valueFrom:
                secretKeyRef:
                  name: {{ include "modelrouter.fullname" (dict "Release" $.Release "Chart" $.Chart "Values" $.Values) }}-secrets
                  key: provider-{{ $provider }}
            {{- end }}
          volumeMounts:
            - name: config
              mountPath: /config
              readOnly: true
            {{- if .Values.persistence.enabled }}
            - name: data
              mountPath: /data
            {{- end }}
          livenessProbe:
            httpGet:
              path: /health
              port: http
            initialDelaySeconds: 5
            periodSeconds: 10
            failureThreshold: 3
          readinessProbe:
            httpGet:
              path: /health
              port: http
            initialDelaySeconds: 3
            periodSeconds: 5
            failureThreshold: 3
          resources:
            {{- toYaml .Values.resources | nindent 12 }}
      volumes:
        - name: config
          configMap:
            name: {{ include "modelrouter.fullname" . }}-config
        {{- if .Values.persistence.enabled }}
        - name: data
          persistentVolumeClaim:
            claimName: {{ include "modelrouter.fullname" . }}-data
        {{- end }}
```

- [ ] **Step 2: Create `deploy/helm/modelrouter/templates/hpa.yaml`**

```yaml
{{- if .Values.autoscaling.enabled }}
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: {{ include "modelrouter.fullname" . }}
  labels:
    {{- include "modelrouter.labels" . | nindent 4 }}
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: {{ include "modelrouter.fullname" . }}
  minReplicas: {{ .Values.autoscaling.minReplicas }}
  maxReplicas: {{ .Values.autoscaling.maxReplicas }}
  metrics:
    {{- if .Values.autoscaling.targetCPUUtilizationPercentage }}
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: {{ .Values.autoscaling.targetCPUUtilizationPercentage }}
    {{- end }}
    {{- if .Values.autoscaling.targetMemoryUtilizationPercentage }}
    - type: Resource
      resource:
        name: memory
        target:
          type: Utilization
          averageUtilization: {{ .Values.autoscaling.targetMemoryUtilizationPercentage }}
    {{- end }}
{{- end }}
```

- [ ] **Step 3: Run `helm lint` to confirm chart is valid**

Run: `helm lint deploy/helm/modelrouter/`
Expected: `1 chart(s) linted, 0 chart(s) failed`

- [ ] **Step 4: Run `helm template` to render the full chart and verify all resources appear**

Run: `helm template test-release deploy/helm/modelrouter/ | grep "^kind:" | sort`
Expected output (order may vary):
```
kind: ConfigMap
kind: Deployment
kind: PersistentVolumeClaim
kind: Secret
kind: Service
```

- [ ] **Step 5: Run `helm template` with autoscaling enabled to verify HPA renders**

Run: `helm template test-release deploy/helm/modelrouter/ --set autoscaling.enabled=true | grep "^kind:" | sort`
Expected: same as above plus `kind: HorizontalPodAutoscaler`

- [ ] **Step 6: Verify init container is present in rendered Deployment**

Run: `helm template test-release deploy/helm/modelrouter/ | grep -A2 "initContainers"`
Expected: output showing `- name: migrate`

- [ ] **Step 7: Run `helm template` with provider secrets to verify env var injection**

Run: `helm template test-release deploy/helm/modelrouter/ --set secrets.providers.openai=sk-test | grep -A4 "MODELROUTER__PROVIDERS"`
Expected: shows env var with secretKeyRef for openai API key

- [ ] **Step 8: Commit**

```bash
git add deploy/helm/modelrouter/templates/deployment.yaml \
        deploy/helm/modelrouter/templates/hpa.yaml
git commit -m "feat: helm chart Deployment with init container and HPA template"
```
