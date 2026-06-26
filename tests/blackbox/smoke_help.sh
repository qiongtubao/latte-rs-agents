#!/usr/bin/env bash
# smoke_help.sh — verify `latte-agent chat --help` lists the flags the
# black-box tests rely on.
#
# Exit 0 on pass, non-zero on fail. No model calls.

set -euo pipefail

BIN="${LATTE_AGENT_BIN:-./target/debug/latte-agent}"
if [[ ! -x "$BIN" ]]; then
  BIN="$(dirname "$0")/../../target/debug/latte-agent"
fi

"$BIN" chat --help > /tmp/latte-smoke-help.out 2>&1

grep -q -- '--debug' /tmp/latte-smoke-help.out \
  || { echo "FAIL: --help missing --debug"; exit 1; }
grep -q -- '--tier' /tmp/latte-smoke-help.out \
  || { echo "FAIL: --help missing --tier"; exit 1; }
grep -qE -- '(-r, --role|-r, --role <ROLE>)' /tmp/latte-smoke-help.out \
  || { echo "FAIL: --help missing -r/--role"; exit 1; }

echo "PASS"