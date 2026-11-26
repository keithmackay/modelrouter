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

{{/*
True when the chart is given a PostgreSQL store: a URL, an existing secret,
or a postgres_url in the rendered config.
*/}}
{{- define "modelrouter.hasPostgres" -}}
{{- if or .Values.database.postgresUrl .Values.database.existingSecret (regexMatch "(?m)^\\s*postgres_url\\s*=" .Values.config) -}}
true
{{- end -}}
{{- end }}

{{/*
Refuse value combinations that cannot survive autoscaling.
*/}}
{{- define "modelrouter.validate" -}}
{{- if .Values.autoscaling.enabled }}
{{- if ne .Values.cluster.backend "redis" }}
{{- fail "autoscaling.enabled requires cluster.backend: redis. The memory backend divides each limit by a replica count the HPA keeps changing, so limits drift as it scales." }}
{{- end }}
{{- if .Values.persistence.enabled }}
{{- fail "autoscaling.enabled requires persistence.enabled: false. The SQLite store on a ReadWriteOnce volume is single-writer; use PostgreSQL (database.postgresUrl or database.existingSecret)." }}
{{- end }}
{{- if not (include "modelrouter.hasPostgres" .) }}
{{- fail "autoscaling.enabled requires a PostgreSQL store: set database.postgresUrl or database.existingSecret, or postgres_url under [database] in config." }}
{{- end }}
{{- if lt (int .Values.autoscaling.minReplicas) 2 }}
{{- fail "autoscaling.minReplicas must be at least 2, or a single pod's crash takes the service down until it is replaced." }}
{{- end }}
{{- if lt (int .Values.autoscaling.maxReplicas) (int .Values.autoscaling.minReplicas) }}
{{- fail "autoscaling.maxReplicas must be at least autoscaling.minReplicas." }}
{{- end }}
{{- if not (or .Values.autoscaling.targetInFlightPerPod .Values.autoscaling.targetCPUUtilizationPercentage .Values.autoscaling.targetMemoryUtilizationPercentage) }}
{{- fail "autoscaling.enabled needs at least one target: targetInFlightPerPod, targetCPUUtilizationPercentage or targetMemoryUtilizationPercentage." }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The replica count each pod divides its fallback limits by. Under the HPA the
live count moves, so use the ceiling: the Redis-down share is then never
larger than intended.
*/}}
{{- define "modelrouter.clusterReplicas" -}}
{{- if .Values.autoscaling.enabled -}}
{{ .Values.autoscaling.maxReplicas }}
{{- else -}}
{{ .Values.replicaCount }}
{{- end -}}
{{- end }}

{{/*
Store env shared by the migrate init container and the router.
*/}}
{{- define "modelrouter.storeEnv" -}}
# Each replica enforces its share of the in-memory limits (under the
# HPA, of maxReplicas).
- name: MODELROUTER_CLUSTER__REPLICAS
  value: {{ include "modelrouter.clusterReplicas" . | quote }}
{{- if .Values.persistence.enabled }}
- name: MODELROUTER_DATABASE__PATH
  value: /data/router.db
{{- end }}
{{- if .Values.database.existingSecret }}
- name: MODELROUTER_DATABASE__POSTGRES_URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.database.existingSecret }}
      key: {{ .Values.database.existingSecretKey }}
{{- else if .Values.database.postgresUrl }}
- name: MODELROUTER_DATABASE__POSTGRES_URL
  valueFrom:
    secretKeyRef:
      name: {{ include "modelrouter.fullname" . }}-secrets
      key: postgres-url
{{- end }}
{{- end }}
