#!/usr/bin/env bash
# Keep this terminal open. Browse to http://127.0.0.1:8765.
set -euo pipefail
exec gcloud compute ssh infinibtc_gmail_com@oasis \
  --project=project-ff5f81e1-75cb-4887-8df --zone=asia-east1-b --tunnel-through-iap \
  -- -NT -L 127.0.0.1:8765:127.0.0.1:8765 -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=30 -o ServerAliveCountMax=3
