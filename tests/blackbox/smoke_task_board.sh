#!/usr/bin/env bash
# smoke_task_board.sh — 任务看板 REST API 生命周期黑盒测试。
#
# 起一个真实的 `latte-agent ui` server（临时 cwd，随机端口），
# 用 curl 走一遍不依赖模型的看板链路：
#   建任务 → 改状态 → 父子嵌套（含孙任务拒绝）→ 排期字段 →
#   列表聚合 → 删除归档 → 磁盘落盘（board.json + <id>.json）
#
# 不发模型调用（dispatch/abort/report 需要真实 session，不在此覆盖）。

set -uo pipefail

BIN="${LATTE_AGENT_BIN:-./target/debug/latte-agent}"
if [[ ! -x "$BIN" ]]; then
  BIN="$(dirname "$0")/../../target/debug/latte-agent"
fi
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

command -v curl >/dev/null || { echo "SKIP: curl not found"; exit 0; }
command -v python3 >/dev/null || { echo "SKIP: python3 not found"; exit 0; }

PORT=$(( 20000 + RANDOM % 20000 ))
BASE="http://127.0.0.1:$PORT/api"
TMP="$(mktemp -d)"
SERVER_LOG="$TMP/server.log"

cleanup() {
  [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null
  rm -rf "$TMP"
}
trap cleanup EXIT

"$BIN" ui --port "$PORT" \
  --agents-config "$REPO_ROOT/config/agents" \
  --models-config "$REPO_ROOT/config/models.toml" \
  --cwd "$TMP" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

# 等 server 就绪（最多 15s）
ready=0
for _ in $(seq 1 30); do
  if curl -sf "$BASE/health" >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.5
done
[[ "$ready" == "1" ]] || { echo "FAIL: ui server not ready"; cat "$SERVER_LOG"; exit 1; }

fail() { echo "FAIL: $1"; exit 1; }

# jsonget <json> <python-expr on d>
jsonget() { python3 -c "import json,sys; d=json.loads(sys.stdin.read()); print($1)"; }

# ── 1. 建任务 → backlog，id 从 LAT-100 起 ──
out=$(curl -sf -X POST "$BASE/tasks" -H 'Content-Type: application/json' \
  -d '{"title":"黑盒：任务生命周期","description":"desc","priority":2}') || fail "create task"
TID=$(echo "$out" | jsonget "d['id']")
[[ "$TID" == "LAT-100" ]] || fail "first task id = $TID, want LAT-100"
[[ "$(echo "$out" | jsonget "d['state']")" == "backlog" ]] || fail "initial state not backlog"
[[ "$(echo "$out" | jsonget "d['history'][0]['to']")" == "backlog" ]] || fail "create history missing"

# ── 2. 改状态 backlog → todo，actions 含 dispatch ──
out=$(curl -sf -X PATCH "$BASE/tasks/$TID" -H 'Content-Type: application/json' -d '{"state":"todo"}') \
  || fail "patch state"
[[ "$(echo "$out" | jsonget "d['state']")" == "todo" ]] || fail "state not todo"
echo "$out" | jsonget "'dispatch' in d['actions']" | grep -q True || fail "todo actions missing dispatch"

# ── 3. 子任务 OK；孙任务必须拒绝（最多一层）──
out=$(curl -sf -X POST "$BASE/tasks" -H 'Content-Type: application/json' \
  -d "{\"title\":\"子任务\",\"parent_id\":\"$TID\"}") || fail "create child"
CID=$(echo "$out" | jsonget "d['id']")
rc=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/tasks" -H 'Content-Type: application/json' \
  -d "{\"title\":\"孙任务\",\"parent_id\":\"$CID\"}")
[[ "$rc" == "400" ]] || fail "grandchild create rc=$rc, want 400"

# ── 4. 排期字段 → actions 切换为已排期形态 ──
out=$(curl -sf -X PATCH "$BASE/tasks/$TID" -H 'Content-Type: application/json' \
  -d '{"scheduled_at":9999999999999}') || fail "patch scheduled_at"
echo "$out" | jsonget "'unschedule' in d['actions']" | grep -q True || fail "scheduled actions missing unschedule"

# ── 5. 列表聚合：父任务 sub_total=1 ──
out=$(curl -sf "$BASE/tasks") || fail "list tasks"
echo "$out" | jsonget "[t for t in d if t['id']=='$TID'][0]['sub_total']" | grep -q '^1$' \
  || fail "parent sub_total != 1"

# ── 6. 删除子任务 → 进 archive/ ──
rc=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/tasks/$CID")
[[ "$rc" == "204" || "$rc" == "200" ]] || fail "delete child rc=$rc"
[[ -f "$TMP/.latte/tasks/archive/$CID.json" ]] || fail "archived child file missing"

# ── 7. 磁盘落盘：board.json + 任务文件 + 原子写无 .tmp 残留 ──
[[ -f "$TMP/.latte/tasks/board.json" ]] || fail "board.json missing"
[[ -f "$TMP/.latte/tasks/$TID.json" ]] || fail "task file missing"
ls "$TMP/.latte/tasks/"*.tmp 2>/dev/null && fail "stray .tmp file"

echo "PASS"
