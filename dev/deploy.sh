#!/usr/bin/env bash
# Builds both images, loads them into the kind cluster from up.sh and (re)deploys the chart with dev/values.yaml.
set -euo pipefail

name=${SLOPPOD_KIND_NAME:-sloppod}
ctx=kind-$name
here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
state=${SLOPPOD_DEV_STATE:-$here/.state}
export HELM_CACHE_HOME=$state/helm/cache HELM_CONFIG_HOME=$state/helm/config HELM_DATA_HOME=$state/helm/data

# What the images are built from, so the runtime reports it and the page it serves compares its own build against it
# (#448), as a published image does.
build=$(git -C "$root" rev-parse HEAD)
for target in runtime session; do
  image=ghcr.io/bo0tzz/sloppod$([[ $target == session ]] && echo -session || true):dev
  docker build --target "$target" --build-arg "SLOPPOD_BUILD=$build" -t "$image" -f "$root/Containerfile" "$root"
  kind load docker-image --name "$name" "$image"
done

helm upgrade --install sloppod "$root/charts/sloppod" --kube-context "$ctx" -n sloppod \
  -f "$here/values.yaml" ${SLOPPOD_DEV_VALUES:+-f "$SLOPPOD_DEV_VALUES"} --wait --timeout 5m
kubectl --context "$ctx" -n sloppod rollout restart deploy/sloppod
kubectl --context "$ctx" -n sloppod rollout status deploy/sloppod --timeout 5m
