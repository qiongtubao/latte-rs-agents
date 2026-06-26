#!/usr/bin/env bash
# smoke_invalid_role.sh — verify the CLI produces a clear error when
# asked to use a non-existent role.
#
# Verifies: stderr/stdout contains "role '...' not found" (the
# exact phrasing the CLI uses in chat.rs / build_runner). No model
# call (the role lookup fires before any model resolution).

set -euo pipefail

BIN="${LATTE_AGENT_BIN:-./target/debug/latte-agent}"
if [[ ! -x "$BIN" ]]; then
  BIN="$(dirname "$0")/../../target/debug/latte-agent"
fi

# Run with the bogus role and a dummy tier+model so config loads,
# but the role resolution itself fires before the model would.
output=$("$BIN" chat -r nonexistent_role -t standard -m deepseek-v4-flash \
  --agents-config "$(dirname "$0")/../../config/agents" \
  --models-config "$(dirname "$0")/../../config/models.toml" 2>&1 || true)

# Match the CLI's error format: "role 'X' not found" — accept either
# 'role "..." not found' or 'role ... not found' style.
echo "$output" | grep -qE 'role .* not found' \
  || { echo "FAIL: missing 'role ... not found' in output:"; echo "$output"; exit 1; }

echo "PASS"