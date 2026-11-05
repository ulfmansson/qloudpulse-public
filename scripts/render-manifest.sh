#!/bin/sh
# Writes manifests/qloudpulse-agent.yaml from the Helm chart, so that the plain YAML and the chart install the same
# thing. Run it after every change to the chart, and commit both.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/manifests/qloudpulse-agent.yaml"
mkdir -p "$root/manifests"

{
  cat <<'HEADER'
# The qloudpulse cluster agent as plain YAML. It installs the same thing as the Helm chart in charts/qloudpulse-agent,
# which this file is generated from by scripts/render-manifest.sh.
#
# Before you apply it, replace these three values, in the Secret and the ConfigMap just below:
#
#   REPLACE-WITH-YOUR-TOKEN         your company's token from qloudpulse
#   REPLACE-WITH-CLUSTER-NAME       on EKS, the EKS cluster's own name
#   https://REPLACE-WITH-ENDPOINT/  the address qloudpulse gives you, ending in /api/v1/write
#
# Then run: kubectl apply -f qloudpulse-agent.yaml
# If you change a value later, also run: kubectl -n qloudpulse rollout restart deployment/qloudpulse-agent
---
apiVersion: v1
kind: Namespace
metadata:
  name: qloudpulse
  labels:
    app.kubernetes.io/name: qloudpulse-agent
HEADER
  helm template qloudpulse-agent "$root/charts/qloudpulse-agent" \
    --namespace qloudpulse \
    --set token=REPLACE-WITH-YOUR-TOKEN \
    --set clusterName=REPLACE-WITH-CLUSTER-NAME \
    --set endpoint=https://REPLACE-WITH-ENDPOINT/api/v1/write |
    sed -e 's/managed-by: Helm/managed-by: kubectl/' \
      -e '/helm\.sh\/chart:/d' \
      -e '/^# Source: /d' \
      -e '/checksum\/config:/d' \
      -e '/checksum\/settings:/d' \
      -e '/^ *annotations:$/d'
} >"$out"

echo "Wrote $out"
