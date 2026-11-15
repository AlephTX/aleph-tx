#!/usr/bin/env bash
# Keep this terminal open. Browse to http://127.0.0.1:8765.
set -euo pipefail
export CLOUDSDK_PYTHON_SITEPACKAGES="${CLOUDSDK_PYTHON_SITEPACKAGES:-1}"
host=${OASIS_HOST:-oasis-tokyo}
zone=${OASIS_ZONE:-asia-northeast1-b}
target=${OASIS_SSH_TARGET:-$host}
gcloud_args=(compute ssh "$target" "--zone=$zone" --tunnel-through-iap)
if [[ -n "${OASIS_GCP_PROJECT:-}" ]]; then
  gcloud_args+=("--project=$OASIS_GCP_PROJECT")
fi
exec gcloud "${gcloud_args[@]}" \
  -- -NT -L 127.0.0.1:8765:127.0.0.1:8765 -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=30 -o ServerAliveCountMax=3
