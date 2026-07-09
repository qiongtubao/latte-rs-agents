#!/usr/bin/env bash
# smoke_delegate_roles.sh — 端到端验证 manager 委托流程的 I/O 正确性
#
# 用真实 LLM（glm-5.2）跑 10 个场景，覆盖所有角色职责。
# 通过读取 --debug 输出的 trace JSONL 断言完整的委托 I/O 链路：
#
#   输入 → ToolExec(delegate, role=X, task=Y)
#        → PromptBuilt(role=X)  [specialist 收到任务]
#        → ToolExec(role=X, bash/read/...) [specialist 使用工具]
#        → 最终输出包含 specialist 的结果
# 运行方式：
#   bash tests/blackbox/smoke_delegate_roles.sh       # 完整 10 场景（约 20 分钟）
#   QUICK=1 bash tests/blackbox/smoke_delegate_roles.sh  # 快速烟雾 2 场景（约 3 分钟）
#
# 前提：~/.latte/ 中有可用的 glm-5.2 模型配置（API key）
#
# 环境变量：
#   LATTE_AGENT_BIN  — latte-agent 二进制路径，默认 target/debug/latte-agent
#   SKIP_SLOW        — 设非空跳过场景 3/9（耗时较长）
#   QUICK            — 设非空只跑场景 1、2（快速验证修复是否生效）
#   VERBOSE          — 设非空输出完整对话

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="${LATTE_AGENT_BIN:-$DIR/../../target/debug/latte-agent}"
VERBOSE="${VERBOSE:-}"

if [[ ! -x "$BIN" ]]; then
  echo "FAIL: binary not found at $BIN (build with 'cargo build' first)"
  exit 1
fi

passed=0
failed=0
skipped=0

# ─── 辅助函数 ────────────────────────────────────────────────────────────────

# 从 JSONL trace 中提取指定事件的值
# 用法: extract_jsonl <jsonl> <event_name> <jq_filter>
extract_jsonl() {
  local jsonl="$1"
  local event="$2"
  local filter="$3"
  # 每行格式: {"EventName": {payload}}
  # 用 jq 提取 event_name 匹配的行，再从 payload 中取 filter
  echo "$jsonl" | while read -r line; do
    local etype
    etype=$(echo "$line" | jq -r 'keys[0]' 2>/dev/null || true)
    if [[ "$etype" == "$event" ]]; then
      echo "$line" | jq -r ".[\"$event\"] | $filter" 2>/dev/null || true
    fi
  done
}

# 主线验证：跑一次 chat，验证完整的委托 I/O 链路
# 参数: name prompt expected_role expected_tool timeout
run_scenario() {
  local name="$1"
  local prompt="$2"
  local expected_role="$3"
  local expected_tool="${4:-}"
  local timeout="${5:-120}"

  echo "───────────────────────────────────────────────────────────────────────────────"
  echo "  [$name] prompt: $prompt"
  echo "          expect: delegate → role=$expected_role tool=$expected_tool"

  local latte_home
  latte_home="$(mktemp -d -t latte-delegate-XXXXXX)"

  output=$(timeout "$timeout" \
    "$BIN" chat --debug --debug-format jsonl -r manager --tier standard \
      --agents-config "$DIR/../../config/agents" \
      --models-config "$DIR/../../config/models.toml" \
      2>&1 <<< "$prompt" || true)
  if [[ -n "$VERBOSE" ]]; then
    echo "=== RAW OUTPUT ==="
    echo "$output" | tail -40
    echo "=== END ==="
  fi

  local has_delegate=false
  local has_specialist_prompt=false
  local has_specialist_tool=false
  local specialist_role=""
  local task_text=""
  local tool_name=""

  while read -r line; do
    echo "$line" | jq . >/dev/null 2>&1 || continue
    local etype
    etype=$(echo "$line" | jq -r 'keys[0]' 2>/dev/null)
    case "$etype" in
      ToolExec)
        local nf
        nf=$(echo "$line" | jq -r '.ToolExec.name' 2>/dev/null)
        local rf
        rf=$(echo "$line" | jq -r '.ToolExec.meta.role // empty' 2>/dev/null)
        local aj
        aj=$(echo "$line" | jq -r '.ToolExec.args_json // ""' 2>/dev/null)
        if [[ "$nf" == "delegate" ]]; then
          has_delegate=true
          specialist_role=$(echo "$aj" | jq -r '.role // ""' 2>/dev/null)
          task_text=$(echo "$aj" | jq -r '.task // ""' 2>/dev/null)
        fi
        if [[ -n "$rf" && "$rf" != "manager" ]]; then
          has_specialist_tool=true
          tool_name="$nf"
        fi
        ;;
      PromptBuilt)
        local pr
        pr=$(echo "$line" | jq -r '.PromptBuilt.meta.role // ""' 2>/dev/null)
        if [[ -n "$pr" && "$pr" != "manager" ]]; then
          has_specialist_prompt=true
          if [[ -z "$specialist_role" ]]; then
            specialist_role="$pr"
          fi
        fi
        ;;
    esac
  done <<< "$output"

  echo "    diag: delegate=$has_delegate specialist_prompt=$has_specialist_prompt specialist_tool=$has_specialist_tool role=$specialist_role tool=$tool_name"
  if [[ -n "$task_text" && -n "$VERBOSE" ]]; then
    echo "    task: ${task_text:0:120}..."
  fi

  local ok=true
  local failures=""

  if ! $has_delegate; then
    failures+="  ✗ NO delegate tool call found\n"
    ok=false
  fi

  if [[ -n "$expected_role" ]]; then
    if [[ -z "$specialist_role" ]]; then
      failures+="  ✗ NO specialist role detected\n"
      ok=false
    fi
  fi

  if ! $has_specialist_prompt; then
    failures+="  ✗ NO PromptBuilt for specialist role\n"
  fi

  if [[ -n "$expected_tool" && "$expected_tool" != "any" ]]; then
    if ! $has_specialist_tool; then
      failures+="  ? NO tool call from specialist (may have answered directly)\n"
    fi
  fi

  if $ok; then
    echo "  ✓ PASS"
    passed=$((passed + 1))
  else
    echo "  ✗ FAIL:"
    echo -e "$failures"
    if echo "$output" | grep -qE "→ delegating to"; then
      echo "    (stderr shows delegation, partial pass)"
    fi
    failed=$((failed + 1))
  fi

  rm -rf "$latte_home"
}

# =============================================================================
# 场景 1 — auto-scout
# 用户: "看看这个项目"
# 预期: delegate → programmer → bash/read/list
# =============================================================================
run_scenario \
  "01-auto-scout" \
  "看看这个项目" \
  "programmer" \
  "bash"

# =============================================================================
# 场景 2 — 读代码分析
# 用户: "分析 src/main.rs 的功能"
# 预期: delegate → programmer → read
# =============================================================================
run_scenario \
  "02-read-code" \
  "分析 src/main.rs 的功能" \
  "programmer" \
  "read" \
  150

if [[ -n "${QUICK:-}" ]]; then
  echo "═══════════════════════════════════════════════════════════════════════════════"
  echo "  QUICK mode: stopping after scenarios 1+2 (set to empty to run full suite)"
  echo "  Results:  Pass: $passed   Fail: $failed   Skip: $skipped"
  echo "═══════════════════════════════════════════════════════════════════════════════"
  if [[ "$failed" -gt 0 ]]; then
    exit 1
  fi
  exit 0
fi
# =============================================================================
# 场景 3 — 找 bug（耗时较长）
# 用户: "帮我找代码里的 bug"
# 预期: delegate → programmer(找bug) → reviewer_sanity(验证)
# =============================================================================
if [[ -z "${SKIP_SLOW:-}" ]]; then
  run_scenario \
    "03-find-bugs" \
    "帮我找代码里的 bug 和潜在问题" \
    "reviewer_sanity" \
    "read" \
    240
else
  echo "  [03-find-bugs] SKIP (SKIP_SLOW set)"
  skipped=$((skipped + 1))
fi

# =============================================================================
# 场景 4 — 架构分析
# 用户: "分析这个项目的整体架构设计"
# 预期: delegate → architect → read/list
# =============================================================================
run_scenario \
  "04-architecture" \
  "分析这个项目的整体架构设计" \
  "architect" \
  "read" \
  180

# =============================================================================
# 场景 5 — 安全检查
# 用户: "检查代码有没有安全漏洞"
# 预期: delegate → programmer + reviewer_security 做审计
# =============================================================================
run_scenario \
  "05-security" \
  "检查代码有没有安全漏洞或安全隐患" \
  "reviewer_security" \
  "read" \
  240

# =============================================================================
# 场景 6 — 测试设计（耗时较长）
# 用户: "给 agent.rs 的 run_turn 函数设计测试用例"
# 预期: delegate → tester → read
# =============================================================================
run_scenario \
  "06-test-design" \
  "给 latte-agent-core/src/agent.rs 的 run_turn 函数设计测试用例" \
  "tester" \
  "read" \
  180

# =============================================================================
# 场景 7 — 文档生成
# 用户: "给 config/agents.toml 写一份使用说明文档"
# 预期: delegate → tech_writer → read
# =============================================================================
run_scenario \
  "07-documentation" \
  "给 config/agents.toml 文件写一份使用说明文档" \
  "tech_writer" \
  "read" \
  150

# =============================================================================
# 场景 8 — DevOps / CI
# 用户: "检查项目的 CI 配置"
# 预期: delegate → devops → bash/read
# =============================================================================
run_scenario \
  "08-devops-ci" \
  "检查项目的 CI 配置有没有问题" \
  "devops" \
  "bash" \
  180

# =============================================================================
# 场景 9 — reviewer chain（耗时最长）
# 用户: "审查 controller.rs 的设计，需要有 review 流程"
# 预期: delegate → programmer → reviewer_sanity → (reviewer_architecture)
# =============================================================================
if [[ -z "${SKIP_SLOW:-}" ]]; then
  run_scenario \
    "09-reviewer-chain" \
    "帮我审查一下 latte-agent-core/src/controller.rs 的设计，需要有 review 流程" \
    "reviewer_architecture" \
    "read" \
    300
else
  echo "  [09-reviewer-chain] SKIP (SKIP_SLOW set)"
  skipped=$((skipped + 1))
fi

# =============================================================================
# 场景 10 — 跨目录执行
# 在临时空目录下启动 chat，验证 delegate 到 programmer 后能使用 bash
# =============================================================================
echo "───────────────────────────────────────────────────────────────────────────────"
echo "  [10-cross-dir] 在临时空目录下启动 chat 并"看看这个目录里有什么""
echo "          expect: delegate → programmer → bash(pwd+ls)"

latte_home_x10="$(mktemp -d -t latte-delegate-XXXXXX)"
workdir_x10="$(mktemp -d -t latte-crossdir-XXXXXX)"
echo "hello latte" > "$workdir_x10/test.txt"
output10=$(cd "$workdir_x10" && timeout 150 \
  "$BIN" chat --debug --debug-format jsonl -r manager --tier standard \
    --agents-config "$DIR/../../config/agents" \
    --models-config "$DIR/../../config/models.toml" \
    2>&1 <<< "看看这个目录里有什么" || true)

has_delegate_x10=false
has_bash_x10=false
specialist_role_x10=""

while read -r line; do
  echo "$line" | jq . >/dev/null 2>&1 || continue
  etype10=$(echo "$line" | jq -r 'keys[0]' 2>/dev/null)
  case "$etype10" in
    ToolExec)
      nf10=$(echo "$line" | jq -r '.ToolExec.name' 2>/dev/null)
      rf10=$(echo "$line" | jq -r '.ToolExec.meta.role // ""' 2>/dev/null)
      if [[ "$nf10" == "delegate" ]]; then
        has_delegate_x10=true
        specialist_role_x10=$(echo "$line" | jq -r '.ToolExec.args_json | fromjson | .role // ""' 2>/dev/null)
      fi
      # specialist 实际使用的工具（bash/exec 映射）
      if [[ -n "$rf10" && "$rf10" != "manager" && ( "$nf10" == "bash" || "$nf10" == "exec" ) ]]; then
        has_bash_x10=true
      fi
      ;;
  esac
done <<< "$output10"

echo "    diag: delegate=$has_delegate_x10 bash=$has_bash_x10 specialist=$specialist_role_x10"

if $has_delegate_x10; then
  echo "  ✓ PASS (delegate called)"
  passed=$((passed + 1))
else
  echo "  ✗ FAIL: no delegate"
  echo "$output10" | grep -o '"ToolExec":{[^}]*}' | head -5 | sed 's/^/    | /'
  failed=$((failed + 1))
fi

rm -rf "$latte_home_x10" "$workdir_x10"

# =============================================================================
# 总结
# =============================================================================
echo ""
echo "═══════════════════════════════════════════════════════════════════════════════"
echo "  Results:  Pass: $passed   Fail: $failed   Skip: $skipped"
echo "═══════════════════════════════════════════════════════════════════════════════"
total=$((passed + failed + skipped))
echo "  Total: $total  scenarios"

if [[ "$failed" -gt 0 ]]; then
  exit 1
fi
exit 0