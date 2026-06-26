#!/usr/bin/env bash
# smoke_list_roles.sh — verify `latte-agent list roles` shows the
# four roles the black-box tests rely on: the manager and the three
# reviewer tiers (sanity / architecture / security).
#
# No model calls — `list roles` just reads the agents.toml.

set -euo pipefail

BIN="${LATTE_AGENT_BIN:-./target/debug/latte-agent}"
if [[ ! -x "$BIN" ]]; then
  BIN="$(dirname "$0")/../../target/debug/latte-agent"
fi

# `list` defaults to roles if no target given? No — it requires a
# target. Use `list roles`.
output=$("$BIN" list roles \
  --agents-config "$(dirname "$0")/../../config/agents" \
  --models-config "$(dirname "$0")/../../config/models.toml" 2>&1)

for r in manager reviewer_sanity reviewer_architecture reviewer_security; do
  echo "$output" | grep -q "$r" \
    || { echo "FAIL: list roles missing $r"; echo "$output"; exit 1; }
done

echo "PASS"