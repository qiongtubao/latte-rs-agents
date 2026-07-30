#!/bin/bash
# latte-agent 配置同步工具
# 将 latte-rs-agents 的配置同步到目标项目（如 latte-code-editor）。
#
# 用法:
#   ./scripts/sync-config.sh                          # 同步到 code-editor（默认）
#   ./scripts/sync-config.sh /path/to/target/project  # 同步到指定项目
#
# 同步内容:
#   - config/agents/*.toml  → .latte/agents.d/
#   - config/prompts/*.md  → .latte/prompts/
#   - .latte/*.toml         → .latte/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SOURCE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# 默认目标：latte-code-editor
TARGET_DIR="${1:-$HOME/Documents/latte/latte-code-editor}"

if [ ! -d "$TARGET_DIR/.latte" ]; then
    echo "❌ 目标项目目录没有 .latte/: $TARGET_DIR"
    echo "用法: $0 [目标项目路径]"
    exit 1
fi

echo "🔄 同步配置: $SOURCE_DIR → $TARGET_DIR"

# 1. 同步 agent 配置
mkdir -p "$TARGET_DIR/.latte/agents.d" "$TARGET_DIR/.latte/agents"
count=0
for f in "$SOURCE_DIR/config/agents/"*.toml; do
    cp "$f" "$TARGET_DIR/.latte/agents.d/"
    cp "$f" "$TARGET_DIR/.latte/agents/"
    count=$((count + 1))
done
echo "  ✅ Agents: $count 个角色配置"

# 2. 同步 prompts（权威源：config/prompts/）
mkdir -p "$TARGET_DIR/.latte/prompts"
count=0
for f in "$SOURCE_DIR/config/prompts/"*.md; do
    cp "$f" "$TARGET_DIR/.latte/prompts/"
    count=$((count + 1))
done
echo "  ✅ Prompts: $count 个提示词文件"

# 3. 同步模型和讨论配置
for f in "$SOURCE_DIR/.latte/"*.toml; do
    [ -f "$f" ] && cp "$f" "$TARGET_DIR/.latte/"
done
echo "  ✅ Configs: 模型/讨论配置"

# 4. 同步 workflow 模板
mkdir -p "$TARGET_DIR/.latte/workflows.d"
count=0
for f in "$SOURCE_DIR/config/workflows/"*.toml; do
    name="$(basename "$f")"
    target="$TARGET_DIR/.latte/workflows.d/$name"
    if [ ! -f "$target" ]; then
        cp "$f" "$target"
        count=$((count + 1))
    fi
done
if [ "$count" -gt 0 ]; then
    echo "  ✅ Workflows: $count 个新模板"
else
    echo "  ✅ Workflows: 已是最新"
fi

# 5. 验证 TOML 文件
errors=0
for f in "$TARGET_DIR/.latte/agents.d/"*.toml; do
    python3 -c "import tomllib; tomllib.load(open('$f','rb'))" 2>/dev/null || {
        echo "  ⚠️  无效 TOML: $(basename $f)"
        errors=$((errors + 1))
    }
done
if [ "$errors" -eq 0 ]; then
    echo "  ✅ 所有 TOML 文件有效"
else
    echo "  ⚠️  $errors 个文件有问题"
fi

# 6. 统计
agent_count=$(ls "$TARGET_DIR/.latte/agents.d/"*.toml 2>/dev/null | wc -l)
prompt_count=$(ls "$TARGET_DIR/.latte/prompts/"*.md 2>/dev/null | wc -l)
echo ""
echo "📊 目标项目配置状态:"
echo "  角色配置: $agent_count 文件"
echo "  提示词:   $prompt_count 文件"
echo ""
echo "✅ 同步完成。重启 latte-agent ui 后生效。"
