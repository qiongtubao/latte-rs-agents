#!/usr/bin/env bash
# 将 latte-agent-bridge/bindings 同步到 agent-ui-core/src/bindings/
# 在 latte-rs-agents 构建后自动调用，确保 TS 类型与 Rust DTO 一致。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC="$REPO_ROOT/latte-agent-bridge/bindings"
DST="$REPO_ROOT/latte-agent-cli/ui/packages/agent-ui-core/src/bindings"

if [ ! -d "$SRC" ]; then
  echo "error: source bindings not found: $SRC"
  exit 1
fi

mkdir -p "$DST"
cp "$SRC"/*.ts "$DST"/

echo "ok: synced bindings to $DST"