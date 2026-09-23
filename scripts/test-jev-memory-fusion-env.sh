#!/usr/bin/env bash
# Unified env harness for:
#   (A) Jev memory management tests (community.4)
#   (B) Fusion multi-model aggregation panel tests (community.5)
#
# Sources local .env, counts provider env names before/after, lists Fusion
# seats (names + key last4 only), runs existing Jev smoke + local fixture
# tests, and runs focused Fusion compose/runner cargo tests when present.
#
# Does NOT invent Phase1 quality/recall pass. Remote probe ≠ Phase1 gate.
# Secrets are never printed in full.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="$ROOT/.env"
PATTERN='PROVIDER|OPENJEV|JEV_|TYPESAFE|ZHIPU|XIAOMI|DEEPSEEK|OLLAMA|FUSION_|OPENCODE|GLM|MODEL'

redact_last4() {
  local v="${1:-}"
  if [ -z "$v" ]; then
    echo "(unset)"
    return
  fi
  echo "len=${#v} last4=...${v: -4}"
}

count_provider_env() {
  printenv 2>/dev/null | grep -Ei "$PATTERN" | cut -d= -f1 | sort -u | wc -l | tr -d ' '
}

list_provider_env_names() {
  printenv 2>/dev/null | grep -Ei "$PATTERN" | cut -d= -f1 | sort -u || true
}

echo "=============================================="
echo "WinWinCode Jev + Fusion provider env harness"
echo "ROOT=$ROOT"
echo "ENV_FILE=$ENV_FILE"
echo "=============================================="

BEFORE_COUNT="$(count_provider_env)"
echo
echo "=== BEFORE source .env ==="
echo "PROVIDER_ENV_COUNT_BEFORE=$BEFORE_COUNT"
list_provider_env_names | sed 's/^/  - /'

if [ ! -f "$ENV_FILE" ]; then
  echo "FAIL: missing $ENV_FILE"
  exit 1
fi

# shellcheck disable=SC1091
set -a
# shellcheck source=/dev/null
. "$ENV_FILE"
set +a

AFTER_COUNT="$(count_provider_env)"
echo
echo "=== AFTER source .env ==="
echo "PROVIDER_ENV_COUNT_AFTER=$AFTER_COUNT"
DELTA=$((AFTER_COUNT - BEFORE_COUNT))
echo "PROVIDER_ENV_DELTA=+$DELTA"
list_provider_env_names | sed 's/^/  - /'

# --- Fusion seat coverage (names + last4 only) ---
echo
echo "=== Fusion seats (names + key last4 only) ==="
SEAT_CONFIGURED=0
SEAT_TOTAL=0
seat_row() {
  local seat="$1"
  local key_name="$2"
  local endpoint_name="$3"
  local model_name="$4"
  local key_val endpoint_val model_val status
  SEAT_TOTAL=$((SEAT_TOTAL + 1))
  key_val="${!key_name:-}"
  endpoint_val="${!endpoint_name:-}"
  model_val="${!model_name:-}"
  if [ -n "$key_val" ] && [ -n "$endpoint_val" ]; then
    status="configured"
    SEAT_CONFIGURED=$((SEAT_CONFIGURED + 1))
  else
    status="missing"
  fi
  printf '  seat=%-12s status=%-10s key=%s(%s) endpoint=%s model=%s\n' \
    "$seat" "$status" "$key_name" "$(redact_last4 "$key_val")" \
    "${endpoint_val:-(unset)}" "${model_val:-(unset)}"
}

seat_row "GLM"      "FUSION_SEAT_GLM_API_KEY"      "FUSION_SEAT_GLM_ENDPOINT"      "FUSION_SEAT_GLM_MODEL"
seat_row "MIMO"     "FUSION_SEAT_MIMO_API_KEY"     "FUSION_SEAT_MIMO_ENDPOINT"     "FUSION_SEAT_MIMO_MODEL"
seat_row "DEEPSEEK" "FUSION_SEAT_DEEPSEEK_API_KEY" "FUSION_SEAT_DEEPSEEK_ENDPOINT" "FUSION_SEAT_DEEPSEEK_MODEL"
seat_row "OPENCODE" "FUSION_SEAT_OPENCODE_API_KEY" "FUSION_SEAT_OPENCODE_ENDPOINT" "FUSION_SEAT_OPENCODE_MODEL"
seat_row "JEV"      "FUSION_SEAT_JEV_API_KEY"      "FUSION_SEAT_JEV_ENDPOINT"      "FUSION_SEAT_JEV_MODEL"

echo
echo "SEAT_COVERAGE=${SEAT_CONFIGURED}/${SEAT_TOTAL}"

echo
echo "=== OpenCode seat detail ==="
if [ -n "${OPENCODE_API_KEY:-}" ]; then
  echo "  OPENCODE: configured key=$(redact_last4 "${OPENCODE_API_KEY}") endpoint=${OPENCODE_BASE_URL:-(unset)} model=${OPENCODE_MODEL:-(unset)}"
  echo "  OLLAMA seats: removed 2026-09-21 (not in .env)"
else
  echo "  OPENCODE: not configured"
fi

echo
echo "=== Jev provider contract (redacted) ==="
echo "  JEV_PROVIDER=${JEV_PROVIDER:-(unset)}"
echo "  JEV_PROVIDER_ID=${JEV_PROVIDER_ID:-(unset)}"
echo "  JEV_ENDPOINT=${JEV_ENDPOINT:-(unset)}"
echo "  JEV_API_KEY=$(redact_last4 "${JEV_API_KEY:-}")"
echo "  JEV_MODEL=${JEV_MODEL:-(unset)}"
echo "  TYPESAFE_ENDPOINT=${TYPESAFE_ENDPOINT:-(unset)}"
echo "  TYPESAFE_API_KEY=$(redact_last4 "${TYPESAFE_API_KEY:-}")"
echo "  OPENJEV_PROVIDER=${OPENJEV_PROVIDER:-(unset)}"
echo "  OPENJEV_ENDPOINT=${OPENJEV_ENDPOINT:-(unset)}"

# --- Jev smoke: remote probe + local cargo/node ---
echo
echo "=== (A) Jev memory management smoke ==="
JEV_SMOKE_RC=0
if [ -x "$ROOT/scripts/test-jev-typesafe.sh" ] || [ -f "$ROOT/scripts/test-jev-typesafe.sh" ]; then
  echo "Running scripts/test-jev-typesafe.sh (remote probe + cargo jev + node replay fixture)..."
  if bash "$ROOT/scripts/test-jev-typesafe.sh"; then
    echo "JEV_SMOKE=ok"
  else
    JEV_SMOKE_RC=$?
    echo "JEV_SMOKE=failed rc=${JEV_SMOKE_RC}"
  fi
else
  echo "JEV_SMOKE=missing scripts/test-jev-typesafe.sh"
  JEV_SMOKE_RC=2
fi

# typesafe smoke already ran remote probe + local cargo/node fixture tests
if [ "$JEV_SMOKE_RC" -ne 0 ] && [ "$JEV_SMOKE_RC" -ne 2 ]; then
  echo "(typesafe smoke already reported remote probe status; local tests were included)"
fi

# --- Fusion compose / parallel model runner tests (exist on main) ---
echo
echo "=== (B) Fusion multi-model aggregation focused cargo tests ==="
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/winwincode-jev-test-target}"
export RUSTC_WRAPPER="${RUSTC_WRAPPER:-}"
FUSION_RC=0
FUSION_TESTS_FOUND=0

run_cargo_filter() {
  local pkg="$1"
  local filter="$2"
  local label="$3"
  echo "--- cargo test -p $pkg $filter --locked --offline ---"
  if cargo test -p "$pkg" "$filter" --locked --offline 2>&1 | tail -40; then
    echo "FUSION_TEST[$label]=ok"
  else
    local rc=$?
    echo "FUSION_TEST[$label]=failed rc=$rc"
    FUSION_RC=1
  fi
}

# Run focused tests serially. Use --lib / named integration tests so cargo does
# not walk every control-plane integration binary after unit tests already pass.
# Cold CARGO_TARGET_DIR can take many minutes on the ORICO workspace volume.

if grep -q "blind_panel_compose_keeps_isolation" "$ROOT/crates/winwincode-control-plane/src/fusion_compose.rs" 2>/dev/null; then
  FUSION_TESTS_FOUND=1
  echo "--- cargo test -p winwincode-control-plane --lib fusion --locked --offline ---"
  if cargo test -p winwincode-control-plane --lib fusion --locked --offline 2>&1 | tail -40; then
    echo "FUSION_TEST[control-plane-fusion-lib]=ok"
  else
    echo "FUSION_TEST[control-plane-fusion-lib]=failed"
    FUSION_RC=1
  fi
else
  echo "FUSION compose unit tests not found in fusion_compose.rs (skip)"
fi

if grep -q "parallel_runner_keeps_partial_success" "$ROOT/crates/winwincode-codex/src/parallel_model_runner.rs" 2>/dev/null; then
  FUSION_TESTS_FOUND=1
  echo "--- cargo test -p winwincode-codex --lib parallel_model --locked --offline ---"
  if cargo test -p winwincode-codex --lib parallel_model --locked --offline 2>&1 | tail -40; then
    echo "FUSION_TEST[codex-parallel-model-lib]=ok"
  else
    echo "FUSION_TEST[codex-parallel-model-lib]=failed"
    FUSION_RC=1
  fi
else
  echo "parallel_model_runner tests not found (skip)"
fi

if [ -f "$ROOT/crates/winwincode-fusion/tests/blind_panel.rs" ]; then
  FUSION_TESTS_FOUND=1
  echo "--- cargo test -p winwincode-fusion --test blind_panel --locked --offline ---"
  if cargo test -p winwincode-fusion --test blind_panel --locked --offline 2>&1 | tail -40; then
    echo "FUSION_TEST[fusion-blind-panel]=ok"
  else
    echo "FUSION_TEST[fusion-blind-panel]=failed"
    FUSION_RC=1
  fi
else
  echo "winwincode-fusion blind_panel integration test not found (skip)"
fi

echo
echo "FUSION_TESTS_FOUND=$FUSION_TESTS_FOUND"
echo "FUSION_TEST_RC=$FUSION_RC"

# --- Document how FUSION_SEAT_* would feed mock ModelPort panel ---
cat <<'DOC'

=== How FUSION_SEAT_* feeds mock ModelPort / Fusion panel (documentation) ===
1. Panel input (FusionInput.provider_candidates) uses product seat ids:
   e.g. candidate id "seat-glm" with provider route "zhipu", model from
   FUSION_SEAT_GLM_MODEL. Same for seat-mimo / seat-deepseek / seat-opencode /
   seat-jev. Ollama seat removed 2026-09-21.
   seat-jev.
2. Unit tests on main inject mock FusionProviderRouter / ModelPort
   (fusion_compose::tests, ParallelModelRunner mock port). They never open
   Provider connections; FUSION_SEAT_* values are not required for those tests.
3. A future live panel harness would map:
   FUSION_SEAT_*_ENDPOINT / _API_KEY / _MODEL  ->  Provider Runtime adapter
   -> FusionProvider::complete OR kernel ModelPort::stream
   -> ParallelModelRunner seats (FusionRunnerSeat.routes)
   -> compose_via_runner_port / answers_from_parallel_model_frames
4. Product panel seats are identities; Provider Runtime routes remain separate
   (see bd memory fusion-02-modelport-boundary-20260920).
DOC

# --- Summary ---
echo
echo "=============================================="
echo "SUMMARY"
echo "=============================================="
echo "PROVIDER_ENV_COUNT_BEFORE=$BEFORE_COUNT"
echo "PROVIDER_ENV_COUNT_AFTER=$AFTER_COUNT"
echo "PROVIDER_ENV_DELTA=+$DELTA"
echo "SEAT_COVERAGE=${SEAT_CONFIGURED}/${SEAT_TOTAL}"
echo "JEV_SMOKE_RC=$JEV_SMOKE_RC"
echo "FUSION_TESTS_FOUND=$FUSION_TESTS_FOUND"
echo "FUSION_TEST_RC=$FUSION_RC"
echo "NOTE: Remote probe + fixture smoke ≠ Phase1 gate."
echo "NOTE: Phase1 (Critical Recall>=99%, Task Success>=baseline / Fusion quality>best single-model) is NOT claimed here."
echo "=============================================="

if [ "$JEV_SMOKE_RC" -ne 0 ] && [ "$FUSION_RC" -ne 0 ]; then
  exit 1
fi
# Partial failures still exit 0 if at least one track produced evidence; parent
# reads SUMMARY lines. Exit non-zero only when both tracks fail hard.
if [ "$JEV_SMOKE_RC" -ne 0 ]; then
  echo "HARNESS_RESULT=partial (Jev smoke rc=$JEV_SMOKE_RC; fusion rc=$FUSION_RC)"
  exit 0
fi
if [ "$FUSION_RC" -ne 0 ]; then
  echo "HARNESS_RESULT=partial (Fusion tests rc=$FUSION_RC; Jev smoke rc=$Jev_SMOKE_RC)"
  exit 0
fi
echo "HARNESS_RESULT=ok"
exit 0
