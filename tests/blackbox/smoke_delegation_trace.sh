#!/usr/bin/env bash
# smoke_delegation_trace.sh — end-to-end smoke that the manager's
# delegation / verdict / no-delegation header appears in output.
#
# Note: this requires DEEPSEEK_API_KEY to actually reach the manager
# model. Without the key, the binary exits fast with a config error
# and we can't observe the delegation header. We treat the absence
# of the key as a SKIP, not a FAIL — the deepseek call is the
# expensive part of the suite and we don't want to gate the cheap
# smoke tests on it.

set -euo pipefail

BIN="${LATTE_AGENT_BIN:-./target/debug/latte-agent}"
if [[ ! -x "$BIN" ]]; then
  BIN="$(dirname "$0")/../../target/debug/latte-agent"
fi

if [[ -z "${DEEPSEEK_API_KEY:-}" ]]; then
  echo "SKIP: DEEPSEEK_API_KEY unset — delegation smoke needs the model"
  exit 0
fi

output=$(echo "hi" \
  | LATTE_HOME="$(mktemp -d -t latte-bb-smoke.XXXXXX)" \
  "$BIN" chat --debug -r manager --tier standard -m deepseek-v4-flash \
    --agents-config "$(dirname "$0")/../../config/agents" \
    --models-config "$(dirname "$0")/../../config/models.toml" \
  2>&1 || true)

# Accept any of: a delegation header, a reviewer verdict, or the
# manager's "Why no delegation" direct-answer marker. All three
# indicate the manager pipeline ran end-to-end.
echo "$output" | grep -qE '(→ delegating to|VERDICT:|Why no delegation)' \
  || { echo "FAIL: no delegation/verdict/no-delegation header in output"; echo "$output"; exit 1; }

echo "PASS"