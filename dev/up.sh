#!/usr/bin/env bash
# Local dev cluster mirroring what sloppod needs from a real one: Cilium with
# CiliumEnvoyConfig support, agent-sandbox, and a Postgres for the runtime.
set -euo pipefail

name=${SLOPPOD_KIND_NAME:-sloppod}
ctx=kind-$name
here=$(cd "$(dirname "$0")" && pwd)
state=${SLOPPOD_DEV_STATE:-$here/.state}
mkdir -p "$state"
export HELM_CACHE_HOME=$state/helm/cache HELM_CONFIG_HOME=$state/helm/config HELM_DATA_HOME=$state/helm/data
k() { kubectl --context "$ctx" "$@"; }

if ! kind get clusters | grep -qx "$name"; then
  kind create cluster --name "$name" --config "$here/kind.yaml" --wait 0s
  docker update --memory "${SLOPPOD_KIND_MEMORY:-6g}" --memory-swap "${SLOPPOD_KIND_MEMORY:-6g}" "$name-control-plane" >/dev/null
fi

# Hosts running WireGuard/Tailscale set src_valid_mark=1, which makes the kernel
# drop every Cilium L7 redirect (ADR 11). Nodes must have it at 0.
docker exec "$name-control-plane" sysctl -qw net.ipv4.conf.all.src_valid_mark=0

helm upgrade --install cilium cilium --repo https://helm.cilium.io --version 1.20.4 \
  --kube-context "$ctx" -n kube-system -f "$here/cilium-values.yaml" --set k8sServiceHost="$name-control-plane" \
  --wait --timeout 10m

k apply --server-side -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v1.0.8/sandbox.yaml
k create namespace sloppod --dry-run=client -o yaml | k apply -f -
k apply -n sloppod -f "$here/postgres.yaml"
k -n agent-sandbox-system rollout status deploy --timeout 5m
k -n sloppod rollout status deploy/postgres --timeout 5m
echo "dev cluster ready: --context $ctx, namespace sloppod"
