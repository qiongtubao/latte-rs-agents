#!/usr/bin/env bash
# smoke_cjk_no_panic.sh — Chinese input must not crash the binary.
#
# Verifies the UTF-8 truncation fix from ad37f91 is still in place.
# Without DEEPSEEK_API_KEY, the turn fails before any model call —
# but the binary still parses CJK, builds the prompt, and emits the
# SessionEnd trace event. We don't need a real model response to
# verify the panic fix.

set -euo pipefail

BIN="${LATTE_AGENT_BIN:-./target/debug/latte-agent}"
if [[ ! -x "$BIN" ]]; then
  BIN="$(dirname "$0")/../../target/debug/latte-agent"
fi

LATTE_HOME_DIR="$(mktemp -d -t latte-bb-cjk.XXXXXX)"
trap 'rm -rf "$LATTE_HOME_DIR"' EXIT

output=$(echo "中文测试 abc" \
  | LATTE_HOME="$LATTE_HOME_DIR" \
  "$BIN" chat --debug -r manager --tier standard -m deepseek-v4-flash \
    --agents-config "$(dirname "$0")/../../config/agents" \
    --models-config "$(dirname "$0")/../../config/models.toml" \
  2>&1 || true)

# Negative assertion first — the panic must not appear.
if echo "$output" | grep -qE 'panicked at|thread .* panicked'; then
  echo "FAIL: panic detected in CJK test"; echo "$output"; exit 1
fi

# Positive assertion: the pipeline at least emitted a "turn request"
# entry (proving the CJK input made it through parsing and the
# session was built). Without DEEPSEEK_API_KEY the turn fails fast,
# so we check for "turn request" (always emitted) instead of
# "turn response" (only emitted on success).
echo "$output" | grep -q 'turn request' \
  || { echo "FAIL: no 'turn request' log line"; echo "$output"; exit 1; }

echo "PASS"