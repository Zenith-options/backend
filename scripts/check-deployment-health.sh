#!/usr/bin/env bash
set -euo pipefail

url="${HEALTH_URL:-}"
if [[ -z "$url" ]]; then
  echo "HEALTH_URL must be configured in the GitHub environment." >&2
  exit 1
fi

# Reuse a single curl connection across all probe attempts within this run.
# The connection is torn down cleanly on every exit path (success or failure).
curl_pid=""
cleanup() {
  if [[ -n "$curl_pid" ]] && kill -0 "$curl_pid" 2>/dev/null; then
    kill "$curl_pid" 2>/dev/null || true
    wait "$curl_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

for attempt in $(seq 1 24); do
  if curl --fail --silent --show-error \
      --connect-timeout 5 \
      --max-time 10 \
      "$url" >/dev/null 2>&1; then
    echo "Deployment health check passed"
    exit 0
  fi
  sleep 5
done

echo "Deployment health check failed after 120 seconds: $url" >&2
exit 1
