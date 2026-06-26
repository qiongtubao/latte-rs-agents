#!/usr/bin/env bash
# run-all.sh — execute every smoke test in tests/blackbox/ in order
# and report pass/fail counts.
#
# Usage: bash tests/blackbox/run-all.sh
#
# Exit code: 0 if every test PASSed or SKIPped, 1 if any FAILED.

set -uo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"

passed=0
failed=0
skipped=0
total=0

shopt -s nullglob
for script in "$DIR"/smoke_*.sh; do
  name=$(basename "$script")
  total=$((total + 1))
  echo "── $name ────────────────────────────"
  # Capture only the script's own stdout/stderr (no header).
  out=$("$script" 2>&1)
  rc=$?
  # Print the script's output under its header for the operator.
  echo "$out"
  # Find the LAST line that starts with PASS / SKIP / FAIL.
  marker=$(echo "$out" | grep -E '^(PASS|SKIP|FAIL)' | tail -n 1 | awk '{print $1}')
  if [[ -z "$marker" ]]; then
    marker="FAIL"
  fi
  case "$marker:$rc" in
    PASS:0)  passed=$((passed + 1)) ;;
    SKIP:0)  skipped=$((skipped + 1)) ;;
    *)       failed=$((failed + 1)) ;;
  esac
done

echo "Total: $total   Pass: $passed   Skip: $skipped   Fail: $failed"

if [[ "$failed" -gt 0 ]]; then
  exit 1
fi
exit 0