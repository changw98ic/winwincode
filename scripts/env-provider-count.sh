#!/usr/bin/env bash
# Count and list provider-related env vars (values redacted).
# Usage: source .env first if you want Jev vars included.

set -euo pipefail
pattern='PROVIDER|OPENJEV|JEV_|TYPESAFE|GLM|ZHIPU|ANTHROPIC|OPENAI|MODEL'

echo "=== Provider-related env NAME list ==="
printenv | grep -Ei "$pattern" | cut -d= -f1 | sort || true

echo
echo "=== COUNT ==="
printenv | grep -Eic "$pattern" || echo 0

echo
echo "=== Values (redacted) ==="
printenv | grep -Ei "$pattern" | while IFS= read -r line; do
  name="${line%%=*}"
  echo "${name}=<set>"
done | sort
