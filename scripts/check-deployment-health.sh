#!/usr/bin/env bash
set -euo pipefail

url="${HEALTH_URL:-}"
if [[ -z "$url" ]]; then
  echo "HEALTH_URL must be configured in the GitHub environment." >&2
  exit 1
fi

for attempt in $(seq 1 24); do
  curl_status=0
  curl --fail --silent --show-error "$url" >/dev/null 2>&1 || curl_status=$?
  if [[ "$curl_status" -eq 0 ]]; then
    echo "Deployment health check passed"
    exit 0
  fi
  sleep 5
done

echo "Deployment health check failed after 120 seconds: $url" >&2
exit 1
