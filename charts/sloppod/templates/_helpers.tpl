{{- define "sloppod.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "sloppod.selectorLabels" -}}
app.kubernetes.io/name: sloppod
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: runtime
{{- end -}}

{{- define "sloppod.labels" -}}
{{ include "sloppod.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "sloppod.sessionServiceAccount" -}}
{{ include "sloppod.fullname" . }}-session
{{- end -}}

{{- define "sloppod.serviceAccountName" -}}
{{ .Values.serviceAccount.name | default (include "sloppod.fullname" .) }}
{{- end -}}

{{- define "sloppod.caSecret" -}}
{{ include "sloppod.fullname" . }}-egress-ca
{{- end -}}

{{/*
The namespaces the runtime watches for the LiteLLM operator's resources (#461): litellm.namespaces, or the release's
own when it's unset. An empty list turns it off.
*/}}
{{- define "sloppod.litellmNamespaces" -}}
{{- if kindIs "invalid" .Values.litellm.namespaces -}}
{{- list .Release.Namespace | toJson -}}
{{- else -}}
{{- .Values.litellm.namespaces | toJson -}}
{{- end -}}
{{- end }}
