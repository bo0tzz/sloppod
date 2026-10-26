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
