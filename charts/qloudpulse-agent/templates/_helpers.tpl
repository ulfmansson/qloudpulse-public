{{- define "qloudpulse-agent.labels" -}}
app.kubernetes.io/name: qloudpulse-agent
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end }}

{{- define "qloudpulse-agent.selectorLabels" -}}
app.kubernetes.io/name: qloudpulse-agent
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/* A label key as a metric label's name: team stays team, app.kubernetes.io/name becomes app_kubernetes_io_name. */}}
{{- define "qloudpulse-agent.labelName" -}}
{{- regexReplaceAll "[^a-zA-Z0-9_]" . "_" -}}
{{- end }}

{{- define "qloudpulse-agent.secretName" -}}
{{- default .Release.Name .Values.existingSecret -}}
{{- end }}
