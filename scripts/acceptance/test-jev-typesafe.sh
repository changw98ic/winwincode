#!/usr/bin/env bash
# Smoke-test Jev remote (TypeSafe SystemOne) using local .env contract.
# Does NOT invent PASS if the endpoint rejects the key.
#
# Real TypeSafe contract (from https://api.typesafe.ai/openapi.json):
#   POST {JEV_ENDPOINT}
#   Authorization: Bearer ${JEV_API_KEY}
#   body: { model, state, questions: { <name>: { type, instructions, criteria? } } }
#   question type: noul | choice | score
#   GET https://api.typesafe.ai/v1/models lists valid model names
#
# Local OpenJev adapter (OpenJevRemoteProvider / OpenJevRemoteSettings) is a
# SEPARATE contract (TOML camelCase providerId/endpoint/apiKey/...). This
# script proves network+key; it does not claim the Rust transport speaks
# SystemOne JSON yet.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck disable=SC1091
[ -f "$ROOT/.env" ] && set -a && . "$ROOT/.env" && set +a

ENDPOINT="${JEV_ENDPOINT:-${TYPESAFE_ENDPOINT:-}}"
KEY="${JEV_API_KEY:-${TYPESAFE_API_KEY:-${OPENJEV_API_KEY:-}}}"

if [ -z "$ENDPOINT" ] || [ -z "$KEY" ]; then
  echo "FAIL: JEV_ENDPOINT / JEV_API_KEY not set (source $ROOT/.env)"
  exit 1
fi

KEY_LAST4="${KEY: -4}"
echo "Endpoint: $ENDPOINT"
echo "Key: set (length ${#KEY}, last4=...${KEY_LAST4})"

AUTH_BASE="https://api.typesafe.ai"
# Derive models URL from endpoint when possible
MODELS_URL="${AUTH_BASE}/v1/models"

echo
echo "=== GET /v1/models ==="
models_code="$(curl -sS -o /tmp/jev-typesafe-models.json -w '%{http_code}' \
  -H "Authorization: Bearer ${KEY}" \
  --max-time 20 \
  "$MODELS_URL" || echo 000)"
echo "HTTP $models_code"
head -c 400 /tmp/jev-typesafe-models.json 2>/dev/null; echo

MODEL="${JEV_MODEL:-}"
if [ -z "$MODEL" ] && [ "$models_code" = "200" ]; then
  MODEL="$(python3 -c 'import json; d=json.load(open("/tmp/jev-typesafe-models.json")); ms=d.get("models") or []; print(ms[0]["name"] if ms else "")' 2>/dev/null || true)"
fi
MODEL="${MODEL:-jev-latest}"
echo "Using model: $MODEL"

# SystemOneRequest — Jev-style noul (yes/true probability) + choice (3-way)
body="$(JEV_MODEL_RESOLVED="$MODEL" python3 - <<'PY'
import json, os
model = os.environ["JEV_MODEL_RESOLVED"]
print(json.dumps({
  "model": model,
  "state": "The verification command exited with code 0.",
  "questions": {
    "still_necessary": {
      "type": "noul",
      "instructions": "This information is still necessary for completing the current task.",
      "criteria": {
        "true": "The information is still necessary for the current task.",
        "false": "The information is no longer necessary."
      }
    },
    "entailment_choice": {
      "type": "choice",
      "instructions": "Does the state entail the hypothesis that this information is still necessary?",
      "criteria": {
        "entailment": "The state entails the hypothesis",
        "neutral": "Neither entails nor contradicts",
        "contradiction": "The state contradicts the hypothesis"
      }
    }
  }
}, ensure_ascii=False))
PY
)"

echo
echo "=== HTTP probe (SystemOneRequest) ==="
code="$(curl -sS -o /tmp/jev-typesafe-probe.json -w '%{http_code}' \
  -X POST "$ENDPOINT" \
  -H "Authorization: Bearer ${KEY}" \
  -H "Content-Type: application/json" \
  --max-time 30 \
  -d "$body" || echo 000)"
echo "HTTP $code"
head -c 800 /tmp/jev-typesafe-probe.json 2>/dev/null; echo

# 2) Unit tests: mock + interface (no network required)
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/winwincode-jev-test-target}"
export RUSTC_WRAPPER="${RUSTC_WRAPPER:-}"
echo
echo "=== cargo jev unit/integration (mock, offline) ==="
cargo test -p winwincode-provider jev --locked --offline 2>&1 | tail -20
cargo test -p winwincode-provider --test jev_openjev --locked --offline 2>&1 | tail -20

# 3) Replay harness (fixture only; never Phase1 production evidence)
echo
echo "=== replay harness (fixture, not Phase1) ==="
node --test tests/jev-session-replay.test.mjs 2>&1 | tail -15

echo
case "$code" in
  200|201) echo "REMOTE_PROBE=ok (auth+payload accepted; see /tmp/jev-typesafe-probe.json)" ;;
  401|403) echo "REMOTE_PROBE=auth_failed — check JEV_API_KEY (last4=...${KEY_LAST4}) / endpoint path" ;;
  000) echo "REMOTE_PROBE=network_error — endpoint unreachable" ;;
  400|422) echo "REMOTE_PROBE=payload_rejected — inspect /tmp/jev-typesafe-probe.json (OpenAPI: model+state+questions{named,type})" ;;
  *) echo "REMOTE_PROBE=http_$code — inspect /tmp/jev-typesafe-probe.json" ;;
esac
echo
echo "NOTE: Remote probe ≠ Phase1 gate. Phase1 still needs real session replay data."
echo "NOTE: Product Device Provider uses WWC_DEVICE_* in isolated runs; Jev remote is OpenJevRemoteSettings (separate)."
