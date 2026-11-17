#!/usr/bin/env bash
# Runs the whole stack on a local Kubernetes cluster (kind).
# Needs: docker, kind, kubectl.  Then open http://localhost:8080
set -euo pipefail
cd "$(dirname "$0")/.."

kind get clusters | grep -qx streamcut || kind create cluster --name streamcut
docker build -t streamcut-api:dev -f api/Dockerfile .
docker build -t streamcut-worker:dev -f worker/Dockerfile .
kind load docker-image streamcut-api:dev streamcut-worker:dev --name streamcut

kubectl apply -k deploy/k8s
kubectl -n streamcut rollout status statefulset/scylla --timeout=5m
kubectl -n streamcut rollout status deployment/api --timeout=5m
kubectl -n streamcut rollout status deployment/worker --timeout=5m
echo "Ready. In another terminal: kubectl -n streamcut port-forward svc/api 8080:80"
