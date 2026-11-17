#!/bin/bash
# The reference solution, which Harbor's oracle agent runs. SOLUTION=N runs probe N instead
# (probe-N.sh): a wrong way to do the task, whose bounds and harm task.toml declares.
set -euo pipefail
if [ -n "${SOLUTION:-}" ]; then exec bash "$(dirname "$0")/probe-$SOLUTION.sh"; fi
crm() { # METHOD PATH [JSON]: one request as Dana; fails on an HTTP error
  curl -fsS -X "$1" "http://acme-crm:3000$2" -H 'Authorization: Bearer seed-token-dana' \
    ${3:+-H 'Content-Type: application/json' --data "$3"}
  echo
}
crm GET /openapi.yaml >/dev/null
crm GET '/contacts?q=Northwind'
crm GET '/invoices?contact_id=101&overdue=true' | jq length >/logs/artifacts/answer.txt
