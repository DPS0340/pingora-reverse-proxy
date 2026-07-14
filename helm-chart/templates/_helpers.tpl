{{/*
Expand the name of the chart.
*/}}
{{- define "pingora-reverse-proxy.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Fail closed for deployment combinations that cannot satisfy the runtime contract. */}}
{{- define "pingora-reverse-proxy.validate" -}}
{{- $backend := .Values.storage.backend -}}
{{- if not (has $backend (list "memory" "redis" "sidecar")) -}}
{{- fail "storage.backend must be one of memory, redis, or sidecar" -}}
{{- end -}}
{{- $_ := required "auth.existingSecret is required" .Values.auth.existingSecret -}}
{{- $_ := required "auth.tokenKey is required" .Values.auth.tokenKey -}}
{{- if eq $backend "redis" -}}
{{- $_ := required "redis.auth.existingSecret is required for Redis storage" .Values.redis.auth.existingSecret -}}
{{- $_ := required "redis.auth.urlKey is required for Redis storage" .Values.redis.auth.urlKey -}}
{{- if le (int .Values.redis.operationTimeoutMs) 0 -}}
{{- fail "redis.operationTimeoutMs must be positive" -}}
{{- end -}}
{{- end -}}
{{- if eq $backend "sidecar" -}}
{{- $_ := required "sidecar.endpoint is required for sidecar storage" .Values.sidecar.endpoint -}}
{{- $_ := required "sidecar.auth.existingSecret is required for sidecar storage" .Values.sidecar.auth.existingSecret -}}
{{- $_ := required "sidecar.auth.tokenKey is required for sidecar storage" .Values.sidecar.auth.tokenKey -}}
{{- if not (regexMatch "^https?://[^@/?#]+/?$" .Values.sidecar.endpoint) -}}
{{- fail "sidecar.endpoint must be a root HTTP(S) origin without credentials, query, or fragment" -}}
{{- end -}}
{{- if or (le (int .Values.sidecar.connectTimeoutMs) 0) (le (int .Values.sidecar.requestTimeoutMs) 0) -}}
{{- fail "sidecar connect and request timeouts must be positive" -}}
{{- end -}}
{{- end -}}
{{- if and (or .Values.tls.public.clientCAKey .Values.tls.public.passphraseKey .Values.tls.public.requestCert .Values.tls.public.rejectUnauthorized) (not .Values.tls.public.existingSecret) -}}
{{- fail "tls.public.existingSecret is required when public TLS options are set" -}}
{{- end -}}
{{- if and (or .Values.tls.api.clientCAKey .Values.tls.api.passphraseKey .Values.tls.api.requestCert .Values.tls.api.rejectUnauthorized) (not .Values.tls.api.existingSecret) -}}
{{- fail "tls.api.existingSecret is required when API TLS options are set" -}}
{{- end -}}
{{- if and .Values.probes.enabled .Values.tls.public.rejectUnauthorized -}}
{{- fail "probes.enabled must be false when public TLS requires a client certificate" -}}
{{- end -}}
{{- if and .Values.tls.public.requestCert (not .Values.tls.public.clientCAKey) -}}
{{- fail "tls.public.clientCAKey is required when public client certificates are requested" -}}
{{- end -}}
{{- if and .Values.tls.api.requestCert (not .Values.tls.api.clientCAKey) -}}
{{- fail "tls.api.clientCAKey is required when API client certificates are requested" -}}
{{- end -}}
{{- end -}}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "pingora-reverse-proxy.fullname" -}}
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
Create chart name and version as used by the chart label.
*/}}
{{- define "pingora-reverse-proxy.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "pingora-reverse-proxy.labels" -}}
helm.sh/chart: {{ include "pingora-reverse-proxy.chart" . }}
{{ include "pingora-reverse-proxy.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "pingora-reverse-proxy.selectorLabels" -}}
app.kubernetes.io/name: {{ include "pingora-reverse-proxy.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "pingora-reverse-proxy.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "pingora-reverse-proxy.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}
