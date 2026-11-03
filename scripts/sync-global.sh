#!/usr/bin/env bash
# =============================================================================
# latte-agent — 把仓库配置同步到全局层（~/.latte/ 或 $LATTE_HOME）
# =============================================================================
# 用法:
#   ./scripts/sync-global.sh            # 同步到 $LATTE_HOME（缺省 ~/.latte）
#   ./scripts/sync-global.sh --dry-run  # 只报告差异，不写文件
#   ./scripts/sync-global.sh --force    # 跳过交互确认，直接覆盖
#
# 同步内容（源 → 目标）:
#   config/workflows/*.toml  → $LATTE_HOME/workflows.d/      （全量覆盖）
#   config/agents/*.toml     → $LATTE_HOME/agents.d/         （保留本机 model_chain）
#   prompts/*.md             → $LATTE_HOME/prompts.d/         （跳过 manager/advisor）
#   .latte/tools.d/*.md      → $LATTE_HOME/tools.d/          （全量覆盖）
#   config/task_types.toml   → $LATTE_HOME/task_types.toml    （全量覆盖）
#
# 源的选取（为什么不是 .latte/ 里的运行时副本）：
#   - workflows：config/workflows/ 是权威版，.latte/workflows.d/ 是它的运行时
#     副本。二者内容本应一致（config/README.md 的编辑约定），实测只有
#     learn_loop 的 description 一处漂移——同步时以 config/ 为准。
#   - roles：config/agents/ 是角色的唯一权威源（含原先只在项目层副本里的
#     proofreader / compaction —— 项目级 .latte/ 副本已全部移除）。
#   - prompts：仓库根 prompts/ 与二进制内置基座 include_str! 同源。
#     config/prompts/ 是历史镜像（实测 tutor.md 落后、proofreader.md 缺失）。
#   - tool docs：config/prompts/tools/ 是权威集（带 SUMMARY/DETAILS 标记，
#     运行时按 <id>.md 匹配工具名）；仓库根 prompts/tools/ 只有 2 个。
#
# **不同步**（刻意）：
#   - 模型编目（config/models.toml → ~/.latte/models.d/）：api_key / base_url
#     是各机器本地的，权威版只写 ${ENV_VAR} 占位。覆盖会冲掉本机凭据。
#   - ~/.latte/prompts.d/manager.md 与 advisor.md：这两个是「基础设施角色」，
#     prompt 由二进制内置基座兜底、用户文件只做**追加**（role.rs
#     is_infrastructure_role）。同步一份拷贝过去会在基座更新后变成陈旧
#     追加——system prompt 里并存两套规则，其中一套是过期的。
#   - ~/.latte/prompts/（无 .d）：运行时**不读**这个目录（全局层只查
#     prompts.d/），留着只是历史残留。
#
# 为什么要单独一个脚本（而不是复用 install.sh）：
#   install.sh 是「给新机器装一份」的一次性安装器，源是 config/；
#   本脚本是「把本仓库的运行配置推到全局层」的同步器，源同时包含
#   config/（权威版）和仓库根的运行时副本（prompts/、.latte/tools.d/），
#   并且**保留本机调优值**（model_chain）。两者语义不同，混在一起会
#   出现「装一次就覆盖调优」的意外。
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

TARGET_DIR="${LATTE_HOME:-$HOME/.latte}"
DRY_RUN=false
FORCE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run|-n) DRY_RUN=true; shift ;;
        --force|-f)   FORCE=true;   shift ;;
        --help|-h)
            sed -n '2,/^# =/{ /^# =/d; s/^# //p; s/^#$//p; }' "$0"
            exit 0
            ;;
        *) echo "Error: unknown flag '$1'. Use --help." >&2; exit 1 ;;
    esac
done

if [[ -t 1 ]]; then
    GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'
    BOLD='\033[1m'; NC='\033[0m'
else
    GREEN=''; YELLOW=''; CYAN=''; BOLD=''; NC=''
fi

info()   { echo -e "  ${GREEN}✓${NC} $1"; }
warn()   { echo -e "  ${YELLOW}⚠${NC}  $1"; }
error()  { echo -e "  ✗ $1" >&2; }
detail() { echo -e "    $1"; }
section(){ echo ""; echo -e "${BOLD}${CYAN}═══ $1 ═══${NC}"; }

mkdir -p "$TARGET_DIR"

# ─── Helpers ───────────────────────────────────────────────────────────────

# 确认是否覆盖一个已存在且内容不同的文件。
# 非交互（无 TTY）或 --force 时直接覆盖，不阻塞。
confirm_overwrite() {
    local what="$1"
    if $FORCE; then
        return 0
    fi
    if [[ ! -t 0 ]]; then
        return 0
    fi
    local answer
    read -r -p "    覆盖 $what ? [y/N] " answer
    [[ "$answer" =~ ^[Yy]$ ]]
}

# 拷贝一个文件（内容相同则跳过；不同则按需确认）。
install_plain() {
    local src="$1" dst="$2" name="$3"
    if [[ ! -f "$src" ]]; then
        warn "skipping $name (source not found: $src)"
        return 0
    fi
    if [[ -f "$dst" ]] && cmp -s "$src" "$dst"; then
        detail "$name already up-to-date"
        return 0
    fi
    if [[ -f "$dst" ]]; then
        if ! confirm_overwrite "$name"; then
            detail "skipped $name"
            return 0
        fi
    fi
    if $DRY_RUN; then
        info "[dry-run] would install $name → ${dst#$HOME/}"
        return 0
    fi
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    info "installed $name → ${dst#$HOME/}"
}

# 拷贝角色配置，但**保留目标已有的 model_chain**（本机调优值）。
#
# 为什么：config/agents/ 的权威版固定 `deepseek-v4-flash` 单链，保证跨机器
# 的确定性解析；本机实测调优出来的链（多模型回退、顺序）价值更高，直接
# 覆盖等于把本机调优抹掉。其余字段（tools / prompt_file / description /
# temperature / model_tier / skills）一律以权威版为准——那些是**行为**，
# 全局层要跟着仓库走。
install_role_preserving_chain() {
    local src="$1" dst="$2" name="$3"
    if [[ ! -f "$src" ]]; then
        warn "skipping $name (source not found: $src)"
        return 0
    fi
    local payload="$src"
    local tmp=""
    if [[ -f "$dst" ]]; then
        # 先算「合并后」的内容（源 + 目标的 model_chain），再跟目标比。
        # 直接比源文件永远不等——目标的 model_chain 是本机调优值，本来就该
        # 与权威版不同，那样每次跑都报 drift，噪音掩盖真正的漂移。
        tmp="$(mktemp)"
        python3 - "$src" "$dst" "$tmp" <<'PYEOF'
import re, sys
src, dst, out = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(src, encoding="utf-8").read()
old = open(dst, encoding="utf-8").read()
m = re.search(r'^model_chain *= *(\[[^\]]*\])', old, re.M)
if m:
    chain = m.group(1)
    if re.search(r'^model_chain *= *\[[^\]]*\]', text, re.M):
        text = re.sub(r'^model_chain *= *\[[^\]]*\]',
                      f'model_chain = {chain}', text, count=1, flags=re.M)
    else:
        text = text.rstrip() + f'\nmodel_chain = {chain}\n'
open(out, "w", encoding="utf-8").write(text)
PYEOF
        if cmp -s "$tmp" "$dst"; then
            detail "$name already up-to-date"
            rm -f "$tmp"
            return 0
        fi
    else
        tmp=""
    fi

    if [[ -f "$dst" ]]; then
        if ! confirm_overwrite "$name"; then
            detail "skipped $name"
            rm -f "$tmp"
            return 0
        fi
        payload="$tmp"
    fi

    if $DRY_RUN; then
        info "[dry-run] would install $name → ${dst#$HOME/}$([[ -f "$dst" ]] && echo ' (preserving local model_chain)')"
        [[ -n "$tmp" ]] && rm -f "$tmp"
        return 0
    fi
    mkdir -p "$(dirname "$dst")"
    cp "$payload" "$dst"
    [[ -n "$tmp" ]] && rm -f "$tmp"
    info "installed $name → ${dst#$HOME/}$([[ -f "$dst" ]] && echo ' (preserved local model_chain)')"
}

# ─── 1. Workflows ──────────────────────────────────────────────────────────
# 全量覆盖：workflow 是**行为脚本**，全局层留着旧版会让 session 跑陈旧流程
# （config/README.md 记录的实锤事故）。全局多出来的旧 workflow 不删除——
# 那是本机可能自定义的，删掉是破坏性的。
section "Workflows → ${TARGET_DIR#$HOME/}/workflows.d/"

wf_count=0
for f in "$REPO_DIR/config/workflows/"*.toml; do
    [[ -e "$f" ]] || continue
    name="$(basename "$f")"
    install_plain "$f" "$TARGET_DIR/workflows.d/$name" "workflow: $name"
    wf_count=$((wf_count + 1))
done
info "$wf_count workflow templates"

# ─── 2. Roles ──────────────────────────────────────────────────────────────
# 权威源：config/agents/（含注释的完整版）+ .latte/agents.d/ 里 config/ 没有的
# 运行时新增角色（proofreader / compaction）。
# 保留目标已有的 model_chain（本机调优值）。
section "Roles → ${TARGET_DIR#$HOME/}/agents.d/"

role_count=0
for f in "$REPO_DIR/config/agents/"*.toml; do
    [[ -e "$f" ]] || continue
    name="$(basename "$f")"
    install_role_preserving_chain "$f" "$TARGET_DIR/agents.d/$name" "role: $name"
    role_count=$((role_count + 1))
done

# 说明：项目级 .latte/ 运行时副本已于 2026-10-31 全部移除，config/agents/
# 是角色的唯一权威源（含原先只在 .latte/agents.d/ 的 proofreader / compaction，
# 它们已并入 config/agents/）。
info "$role_count role configs"

# ─── 3. Prompts ────────────────────────────────────────────────────────────
# 权威源：仓库根 prompts/（与二进制内置基座 include_str! 同源）。
# 跳过 manager/advisor：基础设施角色的 prompt 由内置基座兜底、用户文件
# 只做追加，同步拷贝会变成陈旧追加。
section "Prompts → ${TARGET_DIR#$HOME/}/prompts.d/"

prompt_count=0
for f in "$REPO_DIR/prompts/"*.md; do
    [[ -e "$f" ]] || continue
    name="$(basename "$f")"
    case "$name" in
        manager.md|advisor.md)
            detail "skip $name (infrastructure role — built-in base applies)"
            continue
            ;;
    esac
    install_plain "$f" "$TARGET_DIR/prompts.d/$name" "prompt: $name"
    prompt_count=$((prompt_count + 1))
done
info "$prompt_count prompt files"

# ─── 4. Tool docs ──────────────────────────────────────────────────────────
# 权威源：config/prompts/tools/（工具说明 md，运行时按 <id>.md 匹配工具名）。
# 项目级 .latte/tools.d/ 副本已移除，原先只存在于那里的内容已并入这里。
section "Tool docs → ${TARGET_DIR#$HOME/}/tools.d/"

tool_count=0
for f in "$REPO_DIR/config/prompts/tools/"*.md; do
    [[ -e "$f" ]] || continue
    name="$(basename "$f")"
    install_plain "$f" "$TARGET_DIR/tools.d/$name" "tool doc: $name"
    tool_count=$((tool_count + 1))
done
info "$tool_count tool docs"

# ─── 5. Task types ─────────────────────────────────────────────────────────
# 权威源：config/task_types.toml（注册表）。运行时 bundled 默认 + 项目层
# <cwd>/.latte/task_types.toml 覆盖；全局层目前不被读取，所以这份是给
# 想要全局 task_type 的使用者一个可复制的起点。
section "Task types → ${TARGET_DIR#$HOME/}/task_types.toml"

install_plain "$REPO_DIR/config/task_types.toml" "$TARGET_DIR/task_types.toml" "task types"

# ─── 6. Clean up stale global dirs ─────────────────────────────────────────
# `~/.latte/prompts/`（无 .d）：运行时**不读**（全局层只查 prompts.d/）。
# 它是更早的历史位置，留着只会让人以为改那里有用。
section "Cleaning stale global dirs"

if [[ -d "$TARGET_DIR/prompts" ]]; then
    stale_count=$(find "$TARGET_DIR/prompts" -name '*.md' -type f 2>/dev/null | wc -l | tr -d ' ')
    if $DRY_RUN; then
        info "[dry-run] would remove $stale_count stale file(s) from ${TARGET_DIR#$HOME/}/prompts/ (not read at runtime)"
    else
        rm -rf "$TARGET_DIR/prompts"
        info "removed stale ${TARGET_DIR#$HOME/}/prompts/ ($stale_count files, not read at runtime)"
    fi
fi

# ─── 7. Validate ───────────────────────────────────────────────────────────

if command -v python3 &>/dev/null; then
    FAILED=0
    while IFS= read -r -d '' f; do
        if ! python3 -c "import tomllib; tomllib.load(open('$f','rb'))" 2>/dev/null; then
            error "TOML parse error in: $f"
            FAILED=1
        fi
    done < <(find "$TARGET_DIR/agents.d" "$TARGET_DIR/workflows.d" -name '*.toml' -print0 2>/dev/null)
    if [[ $FAILED -eq 0 ]]; then
        info "all TOML files parse correctly"
    else
        error "some TOML files failed to parse"
        exit 1
    fi
else
    warn "python3 not available — skipped TOML validation"
fi

# ─── Summary ───────────────────────────────────────────────────────────────
section "Summary"
echo ""
if $DRY_RUN; then
    echo -e "  ${YELLOW}Dry run — no files were written.${NC}"
else
    echo -e "  ${GREEN}Global config synced to ${TARGET_DIR}${NC}"
fi
echo ""
echo "  Global layer contents:"
echo "    roles:     $(ls -1 "$TARGET_DIR/agents.d/"*.toml 2>/dev/null | wc -l | tr -d ' ') files"
echo "    workflows: $(ls -1 "$TARGET_DIR/workflows.d/"*.toml 2>/dev/null | wc -l | tr -d ' ') files"
echo "    prompts:   $(ls -1 "$TARGET_DIR/prompts.d/"*.md 2>/dev/null | wc -l | tr -d ' ') files"
echo "    tool docs: $(ls -1 "$TARGET_DIR/tools.d/"*.md 2>/dev/null | wc -l | tr -d ' ') files"
echo ""
echo "  Not synced (by design):"
echo "    - model catalog (api_key / base_url are per-machine)"
echo "    - prompts.d/manager.md, prompts.d/advisor.md (infrastructure roles: built-in base applies)"
echo ""
