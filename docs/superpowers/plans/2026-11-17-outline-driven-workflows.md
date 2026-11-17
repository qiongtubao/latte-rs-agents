# Outline-Driven Workflows Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an outline document alongside the task board: splitting a big task writes a Markdown outline + a task tree with explicit `needs_split` markers, subtask splitting updates both, and learning docs hang off the outline as a tree — with a clickable outline viewer and HTML export on the board.

**Architecture:** Board task tree stays the execution authority; outline Markdown is a second authority for structure/narrative, reconciled by a stable node id that equals `task.id`. A new `latte-agent-core/src/outline.rs` parses/renders a controlled-block Markdown doc and rebinds `tmp:<title>` anchors to real ids after import. Two workflow configs (`outline_split` new, `task_refine` modified) plus a new `outline_writer` role produce/patch the outline. `learn_doc` publishing gains frontmatter and an outline back-link. Frontend reuses the existing `doc_viewer` + markdown-it.

**Tech Stack:** Rust (axum/serde/tokio, `latte-agent-core`, `latte-agent-ui-server`), TOML workflow/role configs, TypeScript + Vite (`latte-agent-cli/ui`), markdown-it, Mermaid. Test with `cargo test -p <crate>` and the UI build.

**Design spec:** `docs/superpowers/specs/2026-11-17-outline-driven-workflows-design.md` (sections referenced as §N; problem→solution table §2.5, P1–P10).

**Compatibility rule (applies to every task):** every new struct field is `#[serde(default)]`. Old `.latte/tasks/*.json` and old plan JSON must keep loading unchanged. `needs_split` "required on split products" is enforced at the **workflow gate** (`require_plan_tasks` + outline gate), NOT by serde — making it serde-required would break every existing plan consumer. This is a deliberate refinement of spec §4.1; P2 is still satisfied because split workflow output cannot omit the field.

---

## File Structure (locked)

**Create:**
- `latte-agent-core/src/outline.rs` — `OutlineNode`/`OutlineDoc`, `parse_outline`, `render_outline`, `rebind_tmp_ids`, `apply_board_fields`(看板→大纲), `validate_unique_ids`, controlled-block preservation.
- `prompts/outline_writer.md` — outline-writing role system prompt.
- `config/agents/outline_writer.toml` — new role config (mirrors `task_planner.toml`).
- `config/workflows/outline_split.toml` — big-task split workflow.
- `docs/outlines/.gitkeep` — outlines directory.

**Modify:**
- `latte-agent-ui-server/src/tasks.rs` — `Task` + `TaskView` + `view()` + `ImportTask` + `import_one` + `refine_task` precondition + `sync` logic, tests at `mod tests` (~4594).
- `latte-agent-core/src/controller.rs` — `PlanTask.needs_split`, plan schema/known keys, `submit_plan_from` passthrough, outline gate, tests at `mod tests` (~10874).
- `latte-agent-core/src/workflow.rs` — `finalize_learn_doc` frontmatter + `link_outline` mechanical step, `LearnSlugPin` reuse.
- `latte-agent-core/src/lib.rs` (or `mod.rs`) — `pub mod outline;` registration (match how `learn_doc` is declared).
- `latte-agent-core/src/learn_doc.rs` — helper to read task parent/order for frontmatter.
- `latte-agent-ui-server/src/handlers.rs` — `POST /api/outlines/:task_id/sync` handler; refine 409 already in `tasks.rs`.
- `latte-agent-ui-server/src/lib.rs` — register the sync route.
- `latte-agent-cli/ui/src/api.ts` — TS `TaskView` new fields.
- `latte-agent-cli/ui/src/task_board.ts` — needs_split chip, outline link, HTML export button, refine guard.
- `latte-agent-cli/ui/src/doc_viewer.ts` — export HTML helper (or add to `markdown.ts`).
- `latte-agent-cli/ui/src/markdown.ts` — `renderMarkdown` reuse for export.
- `config/workflows/task_refine.toml` — `needs_split` in refine prompt + `outline_patch` step before submit.
- `docs/task-board-design.md`, `docs/learn-loop-doc-driven-design.md` — doc updates.

---

## Task 1: Data model — `Task`, `TaskView`, `ImportTask` new fields

**Files:**
- Modify: `latte-agent-ui-server/src/tasks.rs:85-174` (`Task`), `:260-281` (`TaskView`), `:929-977` (`view()`), `:1149-1204` (`ImportTask`), `:1568-1611` (`import_one`)
- Test: `latte-agent-ui-server/src/tasks.rs` `mod tests` (~4594)

- [ ] **Step 1: Write failing tests for new field round-trip and import propagation**

Add to `mod tests` in `tasks.rs`:

```rust
    #[test]
    fn task_new_fields_default_for_old_json() {
        // 旧任务 JSON 没有新字段 → 必须按缺省解析，不能报错。
        let old = serde_json::json!({
            "schema": 1, "id": "LAT-1", "title": "t", "state": "todo",
            "created_at": 1i64, "updated_at": 1i64
        });
        let t: Task = serde_json::from_value(old).expect("old task must parse");
        assert!(!t.needs_split, "缺省 false");
        assert_eq!(t.outline_path, None);
        assert_eq!(t.doc_path, None);
    }

    #[test]
    fn import_one_propagates_needs_split_recursively() {
        let (dir, mut store) = tmp_store();
        let cwd = dir.path();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{
                "title": "parent", "needs_split": true,
                "subtasks": [{ "title": "child", "needs_split": false }]
            }]
        })).unwrap();
        // 真实签名（tasks.rs:1669）：run_import_transaction(store, |store| ..) 两参；
        // import_tasks_into_inner(store, cwd, req) 三参。
        run_import_transaction(&mut store, |s| import_tasks_into_inner(s, cwd, &req))
            .expect("import ok");
        let all = store.list();
        let parent = all.iter().find(|t| t.title == "parent").expect("parent");
        let child = all.iter().find(|t| t.title == "child").expect("child");
        assert!(parent.needs_split);
        assert!(!child.needs_split);
    }
```

（`tmp_store()` 已在 `mod tests` 第 4598 行定义；`store.list()` 见 `tasks.rs:539`。）

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p latte-agent-ui-server task_new_fields_default_for_old_json import_one_propagates_needs_split_recursively 2>&1 | tail -40`
Expected: compile error — `Task`/`ImportTask` have no `needs_split`/`outline_path`/`doc_path` fields.

- [ ] **Step 3: Add fields to `Task`**

In `Task` struct, after `workflow` field (line 115), insert:

```rust
    /// 规划判定：是否仍需继续拆分。与"实际有无子任务"解耦——删掉子任务不丢规划意图。
    #[serde(default)]
    pub needs_split: bool,
    /// 大纲文档路径（项目相对），仅在根任务写入；子孙通过祖先链解析。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline_path: Option<String>,
    /// 学习任务发布后的文档路径（项目相对），如 docs/learn/<slug>.md。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_path: Option<String>,
```

- [ ] **Step 4: Add fields to `ImportTask` and propagate in `import_one`**

In `ImportTask` struct (line 1152), after `workflow`, insert:

```rust
    /// 规划判定：是否仍需继续拆分（拆分 workflow 产出必填；手工导入缺省 false）。
    #[serde(default)]
    pub needs_split: bool,
    /// 大纲文档路径（仅根任务携带）。
    #[serde(default)]
    pub outline_path: Option<String>,
```

In `import_one`, right after `store.create(...)` (line 1588), add:

```rust
    if item.needs_split || item.outline_path.is_some() {
        let t = store.get_mut(&task.id).expect("刚创建");
        t.needs_split = item.needs_split;
        if let Some(p) = &item.outline_path {
            t.outline_path = Some(p.clone());
        }
        store.persist(&task.id)?;
    }
```

(Existing `paths`/`artifact_paths` blocks at 1589-1597 follow the same mutate-then-persist pattern; keep them.)

- [ ] **Step 4b: Fix the one `Task` struct literal (Rust requires all fields)**

`Task` is constructed literally only at `tasks.rs:685` (inside `create`). Add the three fields there:

```rust
            needs_split: false,
            outline_path: None,
            doc_path: None,
```

Verify no other literal remains — run `cargo build -p latte-agent-ui-server` and fix every `missing field` error it reports by adding the same three lines (do not invent values).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p latte-agent-ui-server task_new_fields_default_for_old_json import_one_propagates_needs_split_recursively 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add latte-agent-ui-server/src/tasks.rs
git commit -m "feat(tasks): add needs_split/outline_path/doc_path fields with import propagation"
```

---

## Task 2: `TaskView.effective_outline_path` via ancestor chain

**Files:**
- Modify: `latte-agent-ui-server/src/tasks.rs:260-281` (`TaskView`), `:929-977` (`view()`)
- Test: `latte-agent-ui-server/src/tasks.rs` `mod tests`

- [ ] **Step 1: Write failing test**

```rust
    #[test]
    fn effective_outline_path_resolves_from_ancestor() {
        let (dir, mut store) = tmp_store();
        let _cwd = dir.path();
        let mut root = store.create("r", "", 3, vec![], None, None, None, None, "test").unwrap();
        root.outline_path = Some("docs/outlines/root.md".into());
        store.insert_task_for_test(root.clone());
        let mut child = store.create("c", "", 3, vec![], None, Some(root.id.clone()), None, None, "test").unwrap();
        store.insert_task_for_test(child.clone());
        let view = store.view(&child);
        assert_eq!(view.effective_outline_path.as_deref(), Some("docs/outlines/root.md"));
    }
```

Verify the exact `store.create` signature at `tasks.rs:634-680` before running — pass `None` for `scheduled_at`/`workflow` in the right positions; if the signature has more args, supply them.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p latte-agent-ui-server effective_outline_path_resolves_from_ancestor 2>&1 | tail -40`
Expected: compile error — `TaskView` has no `effective_outline_path`.

- [ ] **Step 3: Add field to `TaskView` and implement resolution**

In `TaskView` (after `effective_workflow`, line 266) add:

```rust
    /// 生效的大纲路径：自身 outline_path 优先，否则沿 parent_id 祖先链向上找。
    pub effective_outline_path: Option<String>,
```

Add a helper near `TaskStore::view`:

```rust
    /// 沿 parent_id 祖先链找最近的 outline_path（防环：最多 tasks.len() 跳）。
    fn effective_outline_path(&self, task: &Task) -> Option<String> {
        let mut cur = task.clone();
        let mut hops = 0;
        loop {
            if let Some(p) = &cur.outline_path {
                return Some(p.clone());
            }
            let parent = cur.parent_id.as_ref()?;
            if hops >= self.tasks.len() { return None; }
            cur = self.tasks.get(parent)?.clone();
            hops += 1;
        }
    }
```

In `view()`'s `TaskView { ... }` literal, add:

```rust
            effective_outline_path: self.effective_outline_path(task),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p latte-agent-ui-server effective_outline_path_resolves_from_ancestor 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add latte-agent-ui-server/src/tasks.rs
git commit -m "feat(tasks): resolve effective_outline_path through ancestor chain"
```

---

## Task 3: `PlanTask.needs_split` + schema + gate

**Files:**
- Modify: `latte-agent-core/src/controller.rs:877-961` (`PlanTask`), `:7533-7589` (schema properties), `:7570-7585` (known keys), `:13625-13640` in `workflow.rs` (submit passthrough)
- Test: `latte-agent-core/src/controller.rs` `mod tests`

- [ ] **Step 1: Write failing test**

```rust
    #[test]
    fn plan_task_needs_split_round_trips_and_is_known_key() {
        // extract_plan_tasks 收 &str（controller.rs:6697），不是 &Value。
        let raw = r#"{"tasks":[{"title":"a","needs_split":true,"subtasks":[{"title":"b","needs_split":false}]}]}"#;
        let parsed = extract_plan_tasks(raw).expect("解析");
        assert!(parsed[0].needs_split);
        assert!(!parsed[0].subtasks[0].needs_split);
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(!unknown_plan_task_keys(v["tasks"].as_array().unwrap())
            .iter().any(|k| k == "needs_split"), "needs_split 必须是已知键");
    }
```

（`extract_plan_tasks(&str) -> Option<Vec<PlanTask>>` 见 `controller.rs:6697`；`unknown_plan_task_keys(&[Value])` 见 `:7470`，同模块 `mod tests` 可直接调用。）

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p latte-agent-core plan_task_needs_split_round_trips_and_is_known_key 2>&1 | tail -40`
Expected: compile error or assertion — no `needs_split` field / it's reported as unknown key.

- [ ] **Step 3: Add field + schema + known key**

In `PlanTask` (after `workflow`, line 903):

```rust
    /// 是否仍需继续拆分。规划字段，与 subtasks 是否为空解耦。
    #[serde(default)]
    pub needs_split: bool,
```

In `plan_task_input_property`, inside the `BTreeMap::from([...])` properties (line 7533), add one entry:

```rust
        (
            "needs_split".into(),
            input_property(
                PropertyType::Boolean,
                "REQUIRED for split plans: true if this task must be broken down further \
                 before it is executable, false if it is a leaf. Independent of subtasks.",
            ),
        ),
```

In `PLAN_TASK_KNOWN_KEYS` collection (near 7577, the set used by `unknown_plan_task_keys`), add `"needs_split"` so it is not flagged unknown.

`submit_plan_from` needs **no change**: `workflow.rs:13625-13632` reads the raw JSON via `find_tasks_fence` and calls `submit_plan_proposal(&json!({ "tasks": tasks }))`, so `needs_split` passes through as raw JSON without a typed rebuild. Verify by re-reading that block; do not add a field-by-field copy.

- [ ] **Step 3b: Fix the five `PlanTask` struct literals**

Adding a field breaks every literal. Add `needs_split: false,` to each:

- `latte-agent-core/src/controller.rs:11153` (helper `plan_task`)
- `latte-agent-core/src/controller.rs:12375`
- `latte-agent-core/src/controller.rs:12387`
- `latte-agent-cli/src/commands/chat.rs:4105`
- `latte-agent-cli/src/commands/chat.rs:4116`

Then run `cargo build -p latte-agent-core -p latte-agent-cli` and fix any remaining `missing field \`needs_split\`` errors the same way.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p latte-agent-core plan_task_needs_split_round_trips_and_is_known_key 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add latte-agent-core/src/controller.rs latte-agent-core/src/workflow.rs
git commit -m "feat(plan): add needs_split to PlanTask schema and known keys"
```

---

## Task 4: `outline.rs` — parse/render/rebind (`tmp:` anchors)

**Files:**
- Create: `latte-agent-core/src/outline.rs`
- Modify: `latte-agent-core/src/lib.rs` (add `pub mod outline;` next to `learn_doc`)
- Test: inline `#[cfg(test)] mod tests` in `outline.rs`

Define the on-disk format exactly as spec §5.2:

```rust
use std::collections::HashMap;

/// 大纲里的一个任务节点。`id` 导入前为 `tmp:<title>`，导入后为真实 task id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineNode {
    pub id: String,
    pub title: String,
    pub needs_split: bool,
    /// `section:body` 受控区块内的正文，机器同步时原样保留。
    pub body: String,
    /// `learn-links` 受控区块内容（行），由学习 workflow 回写。
    pub learn_links: Vec<String>,
    pub children: Vec<OutlineNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineDoc {
    pub task_id: String,
    pub title: String,
    /// 第一个 `<!-- node: -->` 之前的全部原文（frontmatter、人手写的前言）。
    /// render 时原样放回，保证 parse→render 逐字节幂等。
    pub preamble: String,
    pub root: OutlineNode,
}

/// 把 `<!-- node:<id> [-- needs_split]-->` / `<!-- children:start|end -->` /
/// `<!-- section:body:start|end -->` / `<!-- learn-links:start|end -->` 控制的 Markdown
/// 解析成树。锚外自由文本不参与结构，只在 render 时原样放回（保留 `preamble`）。
```

Keep a `preamble: String` field on `OutlineDoc` for text before the first `<!-- node: -->` and store it verbatim, so机器不碰人手写的前言（spec §5.3 "重生成不丢内容"）。

**Round-trip guarantee (be precise — do not over-promise):** `render_outline(parse_outline(x)) == x` holds for **canonical** docs (the shape this project generates: 4-space-free anchors, `### ` headings, blocks in body→learn-links→children order). Hand-edited variants (extra blank lines, `##`/`####` heading levels, anchor spacing variants) are parsed correctly and re-rendered canonically — content is preserved, byte layout may normalize. Store what is needed to avoid lossy round-trips on canonical input:

```rust
pub struct OutlineNode {
    pub id: String,
    pub title: String,
    pub needs_split: bool,
    /// 标题行原文（含 `### ` 前缀）。render 原样输出，避免改变层级。
    /// `None` = 锚点后没有标题行（根节点常见：`## 概述` 属于 body 区块内部，
    /// 不是节点标题）。**不要**给根节点凭空补一个标题。
    pub heading_raw: Option<String>,
    pub body: String,
    /// 该节点是否带 `section:body` 区块——没有就**不要**在 render 时凭空加。
    pub has_body_block: bool,
    pub learn_links: Vec<String>,
    pub has_learn_links_block: bool,
    pub children: Vec<OutlineNode>,
}
```

Implementation: line-scan; a `node:` anchor at depth d opens a node; `children:start/end` change depth; `body`/`learn-links` markers switch the collecting block. Heading capture rule: the first content line **outside** any block, after the anchor, that starts with `#` becomes `heading_raw` (exactly one line, `Some`); if the next thing after the anchor is a block marker or blank lines followed by a marker, `heading_raw` is `None`. Attach nodes to the nearest open parent. Render mirrors parse: preamble verbatim, then per node anchor line (`<!-- node:{id}{ needs_split} -->`), `heading_raw` if `Some`, body block only if `has_body_block`, learn-links block only if `has_learn_links_block`, then children wrapped in `children:start/end` **only if the node has children** (do not invent empty blocks — the second test's input has none).

`rebind_tmp_ids`:

```rust
/// 用 "title -> real id" 把大纲里所有 `tmp:<title>` 锚替换成真实 task id。
/// 返回未绑定的临时锚（调用方据此报错，避免静默留下 tmp:）。
pub fn rebind_tmp_ids(doc: &mut OutlineDoc, title_to_id: &HashMap<String, String>) -> Vec<String> {
    fn walk(n: &mut OutlineNode, map: &HashMap<String, String>, missing: &mut Vec<String>) {
        if let Some(real) = n.id.strip_prefix("tmp:").and_then(|t| map.get(t.trim())) {
            n.id = real.clone();
        } else if n.id.starts_with("tmp:") {
            missing.push(n.id.clone());
        }
        for c in &mut n.children { walk(c, map, missing); }
    }
    let mut missing = Vec::new();
    walk(&mut doc.root, title_to_id, &mut missing);
    missing
}
```

`apply_board_fields(doc, tasks: &[TaskBrief]) -> Result<(), String>`: **看板 → 大纲**方向——按 id 把看板侧的 `title`/`needs_split` 落到文档，`body` 不动；父结构冲突、文档有而看板无的节点报错。反方向（大纲 → 看板）由 Task 8 的 sync 端点负责，两者不可混用。`TaskBrief { id, title, parent_id, needs_split }` 定义在 `outline.rs`，让 core 不依赖 ui-server 类型。

- [ ] **Step 1: Write failing tests**

```rust
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_render_is_idempotent_and_preserves_body() {
            let md = r#"---
schema: 1
task_id: LAT-100
title: 总纲
---
# 总纲

人手写的前言，不能被机器改。

<!-- node:LAT-100 -->
<!-- section:body:start -->
## 概述
正文一段。
<!-- section:body:end -->
<!-- children:start -->
<!-- node:tmp:模块一 needs_split -->
### 模块一
<!-- section:body:start -->
子正文。
<!-- section:body:end -->
<!-- learn-links:start -->
<!-- learn-links:end -->
<!-- children:end -->
"#;
            let doc = parse_outline(md).expect("parse");
            assert_eq!(doc.root.children[0].title, "模块一");
            assert!(doc.root.children[0].needs_split);
            assert_eq!(render_outline(&doc), md, "往返必须逐字节一致");
        }

        #[test]
        fn rebind_tmp_ids_replaces_all_and_reports_missing() {
            let mut doc = parse_outline("<!-- node:LAT-1 -->\n# t\n<!-- children:start -->\n<!-- node:tmp:A -->\n### A\n<!-- children:end -->").unwrap();
            let mut map = HashMap::new();
            map.insert("A".to_string(), "LAT-9".to_string());
            let missing = rebind_tmp_ids(&mut doc, &map);
            assert!(missing.is_empty());
            assert_eq!(doc.root.children[0].id, "LAT-9");
            let mut map2 = HashMap::new();
            let mut doc2 = doc.clone();
            doc2.root.children[0].id = "tmp:B".into();
            assert_eq!(rebind_tmp_ids(&mut doc2, &map2), vec!["tmp:B".to_string()]);
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p latte-agent-core outline::tests 2>&1 | tail -40`
Expected: compile error — `outline` module / types missing.

- [ ] **Step 3: Implement `outline.rs`**

Implement per the code sketches above. Register `pub mod outline;` in `latte-agent-core/src/lib.rs` (find the exact file: `latte-agent-core/src/lib.rs` declares modules like `pub mod learn_doc;`; if the crate root is `src/lib.rs` under a different name, match the `learn_doc` declaration line).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p latte-agent-core outline::tests 2>&1 | tail -40`
Expected: PASS (idempotence test is exact-string — if it fails, fix `render_outline` whitespace until byte-identical; do not weaken the assertion).

- [ ] **Step 5: Commit**

```bash
git add latte-agent-core/src/outline.rs latte-agent-core/src/lib.rs
git commit -m "feat(outline): parse/render controlled-block outline doc with tmp-id rebinding"
```

---

## Task 5: `outline_writer` role + prompt

**Files:**
- Create: `prompts/outline_writer.md`, `config/agents/outline_writer.toml`
- No test (config/prompt only; validated by workflow loading in Task 6)

- [ ] **Step 1: Write the role config**

Create `config/agents/outline_writer.toml`, mirroring `config/agents/task_planner.toml:11-56` exactly in structure:

```toml
# Role: Outline Writer (大纲撰写)
[roles.outline_writer]
id = "outline_writer"
name = "Outline Writer"
description = "把结构化任务树写成受控区块大纲文档，保持锚点与任务一一对应"
category = "planning"
model_tier = "standard"
model_chain = ["deepseek-v4-flash"]
prompt_file = "prompts/outline_writer.md"
temperature = 0.2
tools = ["read"]
icon = "📐"
```

- [ ] **Step 2: Write the prompt**

Create `prompts/outline_writer.md` with these hard rules (verbatim content):

```markdown
# Outline Writer

你把一份 `{"tasks":[...]}` 任务树写成大纲 Markdown。读者是审查者，不是执行者——目标是"一眼看出大任务拆成哪些块、每块要不要再拆、学习进展如何"。

## 输入
你会收到任务树 JSON；若是子任务拆分，还会收到父节点锚 id 与插入位置。

## 输出（严格遵守，引擎按此机械校验）
- 只输出**一个** ```markdown 代码块，里面是完整大纲文档；块外可以写一句话说明。
- 文档骨架必须是：

```markdown
---
schema: 1
task_id: <大任务 id 或 LAT-ROOT>
title: <大任务标题>
---
# <大任务标题>

<!-- node:<大任务 id 或 LAT-ROOT> -->
<!-- section:body:start -->
## 概述
<1-3 段概述：目标、范围、模块划分>
<!-- section:body:end -->

<!-- children:start -->
<!-- node:tmp:<子任务标题原文>[ needs_split] -->
### <子任务标题>
<!-- section:body:start -->
<从该任务 description 派生的 2-4 句：做什么 + 验收口径>
<!-- section:body:end -->
<!-- learn-links:start -->
<!-- learn-links:end -->
<!-- children:end -->
```

- 每个任务**恰好一个** `<!-- node: -->` 锚；子任务在产出阶段用 `tmp:<标题原文>`（`LAT-ROOT` 仅用于尚无 id 的根）。`needs_split` 当且仅当该任务 JSON 的 `needs_split=true`。
- 锚点的标题（`### ` 行）必须与任务 `title` **逐字一致**（引擎按标题对账，不一致会被拒）。
- `section:body` 与 `learn-links` 区块的起止注释必须成对出现；区块外不要写入任何自动生成内容（那部分留给人手维护）。
- 不要输出 Mermaid、表格、裸行号依据；不要发明任务 id（那是导入后引擎回填的）。
```

- [ ] **Step 3: Verify the role config parses**

There is no test named `load_workflow_by_command`; verify by parsing the TOML directly. Add to `latte-agent-core/src/controller.rs`'s `mod tests` (which already reads `config/` — see `controller.rs:12047`):

```rust
    #[test]
    fn outline_writer_role_config_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../config/agents/outline_writer.toml");
        let raw = std::fs::read_to_string(&path).expect("outline_writer.toml 必须存在");
        let v: toml::Value = toml::from_str(&raw).expect("TOML 解析");
        assert_eq!(
            v["roles"]["outline_writer"]["id"].as_str(),
            Some("outline_writer")
        );
        assert_eq!(
            v["roles"]["outline_writer"]["prompt_file"].as_str(),
            Some("prompts/outline_writer.md")
        );
    }
```

Run: `cargo test -p latte-agent-core outline_writer_role_config_parses 2>&1 | tail -20`
Expected: PASS.

Then run `bash scripts/sync-global.sh` so the global runtime layer (`~/.latte/`, per `config/README.md:16,33`) picks up the new role; without it the running app will not see the role.

- [ ] **Step 4: Commit**

```bash
git add prompts/outline_writer.md config/agents/outline_writer.toml
git commit -m "feat(roles): add outline_writer role and prompt"
```

---

## Task 6: `outline_split` workflow (big-task split) — P1/P2/P3

**Files:**
- Create: `config/workflows/outline_split.toml`
- Modify: `latte-agent-core/src/workflow.rs` — new mechanical gate `require_outline_against_plan` (checks anchor titles ↔ plan titles, needs_split matches, no orphan `tmp:`/ids)
- Test: workflow gate unit test in `workflow.rs` or a config-integration test

- [ ] **Step 1: Write the failing gate test**

```rust
    #[test]
    fn outline_gate_rejects_anchor_title_mismatch() {
        let plan = r#"{"tasks":[{"title":"模块一","needs_split":true,"subtasks":[]}]}"#;
        let outline = "<!-- node:LAT-ROOT -->\n# root\n<!-- children:start -->\n<!-- node:tmp:模块二 needs_split -->\n### 模块二\n<!-- children:end -->";
        let err = check_outline_against_plan(plan, outline).unwrap_err();
        assert!(err.contains("模块一"), "报错要点名缺失/多余的标题: {err}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p latte-agent-core outline_gate_rejects_anchor_title_mismatch 2>&1 | tail -40`
Expected: compile error — function missing.

- [ ] **Step 3: Implement the gate**

In `workflow.rs`, implement `check_outline_against_plan(plan_json: &str, outline_md: &str) -> Result<(), String>`:
- parse plan tasks via existing `parse_and_validate_plan_tasks` (`controller.rs:7630`);
- `outline::parse_outline(outline_md)`;
- compare the multiset of `### {title}` under anchors against plan task titles (top level + nested by `children` depth); on mismatch return `Err(format!("大纲锚点标题与任务清单不一致：..."))` listing missing/extra titles;
- require `needs_split` on every plan task; absent → `Err("任务 <title> 缺少 needs_split")` (see the shared presence check below);
- require outline node `needs_split` flags equal plan's.

**Presence must be checked on raw JSON, not the typed struct.** `PlanTask.needs_split` has `#[serde(default)]`, so an omitted field and an explicit `false` look identical after parsing — checking the typed value would let a model silently skip the field. Implement:

```rust
/// 每个 plan 任务（含任意深度 subtasks）都必须**显式**声明 needs_split。
/// 必须查原始 JSON 键：`PlanTask.needs_split` 有 serde(default)，解析后
/// 「没写」和「写了 false」无法区分。
fn check_plan_tasks_have_needs_split(plan_json: &str) -> Result<(), String> {
    fn walk(v: &serde_json::Value, missing: &mut Vec<String>) {
        let Some(obj) = v.as_object() else { return };
        let title = obj.get("title").and_then(|t| t.as_str()).unwrap_or("<无标题>");
        if !obj.contains_key("needs_split") {
            missing.push(title.to_string());
        }
        if let Some(subs) = obj.get("subtasks").and_then(|s| s.as_array()) {
            for s in subs {
                walk(s, missing);
            }
        }
    }
    let v: serde_json::Value =
        serde_json::from_str(plan_json).map_err(|e| format!("任务清单不是合法 JSON：{e}"))?;
    let mut missing = Vec::new();
    if let Some(arr) = v.get("tasks").and_then(|t| t.as_array()) {
        for t in arr {
            walk(t, &mut missing);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("以下任务缺少 needs_split（拆分类清单必填）：{}", missing.join("、")))
    }
}
```

`check_outline_against_plan` calls this first, then does the title/flag comparison.

Wire it as new step fields, in this order (order matters — `WorkflowStepDef` is `#[serde(deny_unknown_fields)]`, so a TOML key fails to load until the Rust field exists):

1. Add to `WorkflowStepDef` (near `require_plan_tasks`, ~`workflow.rs:2754`):

```rust
    /// 大纲机械校验：要求本步产出的 markdown fence 与 `require_plan_tasks` 的任务树
    /// 在锚点标题 / needs_split 上一致（见 `check_outline_against_plan`）。
    #[serde(default)]
    pub require_outline_against_plan: bool,
    /// 清单机械校验：要求每个 plan 任务显式带 `needs_split`（缺字段 = 规划意图
    /// 不明，绝不能默认成叶子）。拆分类 workflow 必须打开它。
    #[serde(default)]
    pub require_needs_split: bool,
```

2. In the serial runner's mechanical-gate block (where `check_plan_tasks_output` is called, `workflow.rs:524`), after the plan gate, call `check_outline_against_plan(&vars["draft"], &step_output)` when the flag is set.

3. Only then create/load the TOML in Step 5.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p latte-agent-core outline_gate_rejects_anchor_title_mismatch 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 5: Write the workflow config**

Create `config/workflows/outline_split.toml` with 4 steps (copy the exact TOML idioms from `task_refine.toml:54-287`):

```toml
name = "outline_split"
command = "/outline-split"
description = "大任务拆分：出任务树(含 needs_split) + 受控大纲文档，导入看板并回填真实 id｜落点：任务看板 + docs/outlines/<slug>.md"
max_rounds = 1

[[steps]]
id = "breakdown"
description = "task_planner 出结构化拆分草案（含 needs_split；本步不调 plan 工具）"
speakers = ["task_planner"]
output_key = "draft"
max_retries = 1
tools = ["read", "search", "code_graph"]
require_plan_tasks = true
# 拆分类清单必须显式声明 needs_split：缺字段 = 规划意图不明，不能默默当叶子。
require_needs_split = true
prompt = """拆分下面这条大任务：

【大任务】
{{topic}}

先读代码/文档摸清现状再拆。输出：1) 拆分依据（每条引「文件路径 + 符号名」）；2) **恰好一个** ```json 代码块 {"tasks":[...]}。每个任务必须含 title / description(做什么+验收标准) / priority / labels / depends_on / task_type / workflow / paths，且**必须含 needs_split**（true=还需继续拆，false=叶子）——引擎会机械校验，缺字段整单打回。其余拆分纪律沿用 task_planner 角色 prompt。"""

[steps.output_contract]
min_chars = 200
forbid = ["TBD", "待补充", "占位符"]

[[steps]]
id = "outline_draft"
description = "outline_writer 按任务树写受控大纲文档（tmp: 临时锚）"
speakers = ["outline_writer"]
output_key = "outline"
max_retries = 1
tools = []
require_outline_against_plan = true
prompt = """把这份任务树写成大纲 Markdown（严格按你的角色 prompt 的受控区块格式；子任务锚用 tmp:<标题原文>）：

【任务树】
{{draft}}

只输出一个 ```markdown 代码块。"""

[[steps]]
id = "gate"
description = "reviewer 终审：覆盖度/粒度/大纲可读性"
speakers = ["reviewer"]
output_key = "approved"
loop_until = "VERDICT: ACCEPT"
loop_back_to = "breakdown"
loop_abort_on = "VERDICT: REJECT"
max_iterations = 2
prompt = """评审任务树与大纲：覆盖度、粒度、依赖、needs_split 是否判对、大纲是否讲清模块划分。只输出 VERDICT: ACCEPT/REVISE/REJECT 与证据化问题清单，不要重写草案。机械校验（锚点/标题/needs_split 一致）已由引擎完成，不必你复核。"""

[steps.output_contract]
require = ["VERDICT:"]

[[steps]]
id = "submit"
description = "引擎机械提交：抽 tasks JSON 提 plan 导入看板 + 落盘大纲 + rebind_tmp_ids"
submit_plan_from = "draft"
```

**Definite finalization path (no deferred design):** the workflow cannot write the final outline itself, because `submit_plan_from` only *proposes* a plan and real ids exist only after the user imports. So:

1. In the `submit` step, additionally write the outline draft to a session-scoped file and record `plan_id → draft_path` in the same side table that already maps refine sessions to parents (`refine_parents`, `tasks.rs:1799-1809`). Add alongside it:

```rust
/// plan_id → 大纲草案路径（拆分 workflow 的 submit 步登记），导入时据此定稿。
#[derive(Default)]
pub struct OutlineDraftRegistry { pub by_plan: HashMap<String, String> } // plan_id -> rel path
```

2. Extend `ImportTasksRequest` with:

```rust
    /// 大纲草案路径（拆分 submit 步登记；导入时按真实 id 回填并定稿）。
    #[serde(default)]
    pub outline_draft_path: Option<String>,
```

3. In `import_tasks_into_inner`, after all tasks are created and the `by_title` map is complete, if `outline_draft_path` is `Some`, call `outline::rebind_tmp_ids` with that map, then write `docs/outlines/<slug>.md` (tmp+rename), and set the root task's `outline_path` to it. Return an error (aborting the import) if `rebind_tmp_ids` reports any missing `tmp:` anchor — never silently leave `tmp:` in a published outline.

4. The user cancelling import naturally writes nothing (no import call ⇒ no outline).

The slug is derived from the topic title: lowercase, non-alphanumerics → `-`, trimmed; store it in the draft's frontmatter so import reads it back rather than recomputing.

Implement steps 1–3 as code in Task 6 Step 5; the workflow TOML keeps only `submit_plan_from = "draft"` (no `outline_submit_from` key — do not invent a config field the engine does not read).

- [ ] **Step 6: Run the workflow validation smoke**

Add this test to `workflow.rs` `mod tests` (same pattern as the existing all-configs tests at `:16880`):

```rust
    #[test]
    fn outline_split_workflow_is_valid() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../config/workflows/outline_split.toml");
        let raw = std::fs::read_to_string(&path).expect("outline_split.toml 必须存在");
        // WorkflowStepDef 是 deny_unknown_fields：拼错字段名这里就炸。
        let wf: WorkflowDef = toml::from_str(&raw).expect("解析");
        wf.validate().expect("校验");
        let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["breakdown", "outline_draft", "gate", "submit"]);
    }
```

Run: `cargo test -p latte-agent-core outline_split_workflow_is_valid 2>&1 | tail -20`
Expected: PASS. (No project-level copy exists — `.latte/workflows.d/` is absent, so `workflow_copies_do_not_drift` skips; do not create a second copy.)

Then `bash scripts/sync-global.sh` so the runtime layer sees the new workflow.

- [ ] **Step 7: Commit**

```bash
git add config/workflows/outline_split.toml latte-agent-core/src/workflow.rs
git commit -m "feat(workflow): add outline_split with outline gate and id rebind on submit"
```

---

## Task 7: `task_refine` needs_split + import-time child anchors (subtask split) — P5

**Files:**
- Modify: `config/workflows/task_refine.toml:69-124` (refine prompt only — **no new step**)
- Modify: `latte-agent-ui-server/src/tasks.rs:3141-3203` (`refine_task` precondition: reject `needs_split=false`), `import_tasks_into_inner` (child anchor insertion)
- Modify: `latte-agent-core/src/outline.rs` (`insert_child`)
- Test: `tasks.rs` `mod tests` for the 409 precondition + anchor insertion

- [ ] **Step 1: Write failing test for refine guard**

```rust
    #[test]
    fn refine_rejects_leaf_task() {
        let (_dir, mut store) = tmp_store();
        // create 默认 needs_split=false → 叶子，必须拒绝拆分。
        let leaf = store.create("leaf", "", 3, vec![], None, None, None, None, "test").unwrap();
        assert!(matches!(check_refine_allowed(&leaf), Err(RefineError::Leaf)));

        // needs_split=true → 允许拆分。
        let mut parent = store.create("parent", "", 3, vec![], None, None, None, None, "test").unwrap();
        parent.needs_split = true;
        assert!(check_refine_allowed(&parent).is_ok());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p latte-agent-ui-server refine_rejects_leaf_task 2>&1 | tail -40`
Expected: compile error — function missing.

- [ ] **Step 3: Implement the guard**

In `tasks.rs`, add `RefineError`/`check_refine_allowed` and call it at the top of `refine_task` (~3141):

```rust
#[derive(Debug)]
pub enum RefineError { Leaf }

pub fn check_refine_allowed(task: &Task) -> Result<(), RefineError> {
    if task.needs_split { Ok(()) } else { Err(RefineError::Leaf) }
}
```

Map `RefineError::Leaf` to `(StatusCode::CONFLICT, "叶子任务不可再拆（needs_split=false）")` in `handlers.rs:1309-1317`.

- [ ] **Step 4: Add needs_split to refine prompt + turn on the mechanical gate**

In `config/workflows/task_refine.toml`, inside the `refine` step prompt's JSON field list (line 77-85), add `needs_split（必填：true=还需继续拆，false=叶子）` after `workflow(...)`, and add one sentence: "每个子任务必须显式写 needs_split——引擎机械校验，缺失整单打回。"

Also add to the `refine` step's header (next to its existing `require_plan_tasks = true` at `task_refine.toml:66`):

```toml
# 子任务也必须判"要不要再拆"：缺字段 = 规划意图不明，不能默默当叶子。
require_needs_split = true
```

This is what makes the prompt's "必填" true rather than aspirational — `check_plan_tasks_have_needs_split` (Task 6) rejects the draft mechanically. `WorkflowStepDef` already has the field from Task 6, so `deny_unknown_fields` accepts it.

- [ ] **Step 5: Insert child anchors mechanically at import (no model step)**

Do **not** add an `outline_patch` workflow step. `WorkflowStepDef` is `#[serde(deny_unknown_fields)]`, and a model-driven patch would need template vars the engine does not supply. Instead, extend the import-time hook from Task 6:

After `import_tasks_into_inner` creates the child tasks for a refine import (`req.parent_id` is `Some`), resolve the parent's `effective_outline_path`:

```rust
// 子任务拆分：把已建好的子任务作为锚点插入父节点 children 区块。
if let Some(parent_id) = &req.parent_id {
    if let Some(outline_rel) = store.effective_outline_path(parent_by_id) {
        let path = cwd.join(&outline_rel);
        let mut doc = outline::parse_outline(&std::fs::read_to_string(&path)?)?;
        for id in &created {
            let child = store.get(id).expect("刚创建");
            outline::insert_child(
                &mut doc,
                parent_id,
                &child.id,
                &child.title,
                child.needs_split,
                &child.description,
            )?;
        }
        std::fs::write(&path, outline::render_outline(&doc))?; // tmp+rename
    }
}
```

Add `outline::insert_child(doc, parent_id, child_id, title, needs_split, body) -> Result<(), String>`: locates the node whose `id == parent_id` (error with the id if absent — never silently no-op), appends a child carrying a **real** id (children created by import already have ids), and creates the `learn-links` block empty. Idempotence: if a child id already exists under that parent, update title/body instead of duplicating.

This satisfies P5 ("同时更新看板与大纲") with zero model calls and no new config key.

- [ ] **Step 6: Run tests + config smoke**

Run: `cargo test -p latte-agent-ui-server refine_rejects_leaf_task 2>&1 | tail -40` → PASS.
Run: `bash scripts/sync-global.sh && cargo test -p latte-agent-core load_workflow 2>&1 | tail -20` → workflow loads (`deny_unknown_fields` must not see any new key — the TOML only gained no new keys in this task).

- [ ] **Step 7: Commit**

```bash
git add config/workflows/task_refine.toml latte-agent-ui-server/src/tasks.rs latte-agent-ui-server/src/handlers.rs latte-agent-core/src/outline.rs
git commit -m "feat(refine): require needs_split and insert child outline anchors on import"
```

---

## Task 8: Sync API `POST /api/outlines/:task_id/sync` — P6

**Files:**
- Modify: `latte-agent-ui-server/src/handlers.rs` (new handler near `refine_task`), `latte-agent-ui-server/src/lib.rs:759` (route), `latte-agent-core/src/outline.rs` (`sync_with_tasks` already there), `latte-agent-ui-server/src/tasks.rs` (reconcile helper + tests)

- [ ] **Step 1: Write failing test for structure-only sync**

```rust
    #[test]
    fn outline_sync_updates_title_without_touching_state() {
        let (dir, mut store) = tmp_store();
        let _cwd = dir.path();
        // create 不接受 id：id 由 next_seq 生成，必须回读。
        let mut t = store.create("旧标题", "", 3, vec![], None, None, None, None, "test").unwrap();
        t.state = "in_progress".into();
        let id = t.id.clone();
        store.insert_task_for_test(t);
        // sync_struct_from_outline 只改 title/parent_id/needs_split
        sync_struct_from_outline(&mut store, &id, "新标题", None, true).unwrap();
        let got = store.get(&id).unwrap();
        assert_eq!(got.title, "新标题");
        assert_eq!(got.state, "in_progress", "执行状态不能被大纲同步改动");
        assert!(got.needs_split);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p latte-agent-ui-server outline_sync_updates_title_without_touching_state 2>&1 | tail -40`
Expected: compile error — helper missing.

- [ ] **Step 3: Implement reconcile + handler + route**

In `tasks.rs`, implement `sync_struct_from_outline(store, id, title, parent_id, needs_split)` using `store.get_mut` + `store.persist` only for the whitelist fields (`title`, `parent_id`, `needs_split`), never `state`/`runs`/`history`. Conflict rules (spec §7.3): on delete-vs-modify, move the deleted node's body to `docs/outlines/.trash/<id>-<ts>.md` using `std::fs::write`; on missing id with a trash hit, return an error asking the user to restore-or-create. Timestamp ordering via `task.updated_at`.

Handler in `handlers.rs`:

```rust
/// `POST /api/outlines/:task_id/sync` —— 手改大纲后把结构同步回看板（只动结构字段）。
pub(crate) async fn sync_outline(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
    axum::extract::Json(req): axum::extract::Json<crate::tasks::OutlineSyncRequest>,
) -> Result<Json<crate::tasks::TaskView>, (StatusCode, String)> {
    // req: { title?: String, parent_id?: Option<String>, needs_split?: bool }
    crate::tasks::sync_outline(&state.backend, &id, req).map(Json).map_err(Into::into)
}
```

Route in `lib.rs` after `/tasks/:id/workflow-runs/:wf_id` (line 768):

```rust
        .route("/outlines/:task_id/sync", post(sync_outline))
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p latte-agent-ui-server outline_sync_updates_title_without_touching_state 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add latte-agent-ui-server/src/tasks.rs latte-agent-ui-server/src/handlers.rs latte-agent-ui-server/src/lib.rs
git commit -m "feat(api): add outline structure sync endpoint with state-preserving reconcile"
```

---

## Task 9: Frontend fields — outline link, needs_split chip, HTML export — P4/P9

**Files:**
- Modify: `latte-agent-cli/ui/src/api.ts:1437-1478`, `task_board.ts` (imports ~11-16, `renderCard` ~1612, drawer `propSec` ~546-578), `markdown.ts`

- [ ] **Step 1: Add TS fields**

In `TaskView` interface, after `effective_workflow` (line 1473), add:

```ts
  /** 规划判定：是否仍需继续拆分（false=叶子，禁止再拆）。 */
  needs_split: boolean;
  /** 大纲文档路径（仅根任务，项目相对）；子孙用 effective_outline_path。 */
  outline_path: string | null;
  /** 学习任务发布后的文档路径。 */
  doc_path: string | null;
  /** 生效的大纲路径（服务端沿祖先链解析）。 */
  effective_outline_path: string | null;
```

- [ ] **Step 2: needs_split chip in `renderCard`**

In `renderCard`'s `meta` row (after the `tb-chip-workflow` block, ~1628), add:

```ts
    if (task.needs_split) {
      meta.appendChild(el("span", "tb-chip tb-chip-split", "🪓 待拆"));
    }
```

- [ ] **Step 3: Outline link + export button in drawer**

In `renderDrawer`'s `propSec` (before `body.appendChild(propSec)` at line 578), add — reusing the exact session-link pattern at 546-559:

```ts
    if (task.effective_outline_path) {
      const row = el("div", "tb-kv");
      row.appendChild(el("span", "tb-k", "任务大纲"));
      const v = el("span", "tb-v");
      const link = el("a", "doc-link", "打开大纲 ↗") as HTMLAnchorElement;
      link.dataset.docPath = task.effective_outline_path;
      v.appendChild(link);
      const exp = el("button", "tb-btn tb-btn-ghost tb-btn-sm", "导出 HTML") as HTMLButtonElement;
      exp.addEventListener("click", async (e) => {
        e.stopPropagation();
        const { exportOutlineHtml } = await import("./markdown");
        await exportOutlineHtml(task.effective_outline_path!);
      });
      v.appendChild(exp);
      row.appendChild(v);
      propSec.appendChild(row);
    }
    propSec.appendChild(kv("需再拆", task.needs_split ? "是" : "否（叶子）"));
```

- [ ] **Step 4: Implement `exportOutlineHtml`**

In `markdown.ts`, add (uses existing `renderMarkdown` + `/api/doc?path=`):

```ts
/** 取 MD 大纲 → 渲染成自包含只读 HTML → 新窗口打开（D3：MD 权威，HTML 只读）。 */
export async function exportOutlineHtml(path: string): Promise<void> {
  const res = await fetch(`/api/doc?path=${encodeURIComponent(path)}`);
  if (!res.ok) throw new Error(`读大纲失败：${res.status}`);
  const md = await res.text();
  const html = `<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<title>任务大纲（导出）</title><style>
body{max-width:960px;margin:40px auto;padding:0 24px;font-family:-apple-system,"PingFang SC",sans-serif;
line-height:1.8;color:#1f2328}h1,h2,h3{border-bottom:1px solid #d0d7de;padding-bottom:6px}
pre{background:#f6f8fa;padding:12px;border-radius:6px;overflow:auto}table{border-collapse:collapse}
td,th{border:1px solid #d0d7de;padding:6px 10px}</style></head><body>${renderMarkdown(md)}</body></html>`;
  const url = URL.createObjectURL(new Blob([html], { type: "text/html" }));
  window.open(url, "_blank");
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}
```

- [ ] **Step 5: Verify UI build**

Run: `cd latte-agent-cli/ui && npm run build 2>&1 | tail -30` (or `bun run build` if configured in `package.json`)
Expected: TypeScript compile + Vite build succeed. Then launch the app and open a task with an outline to visually confirm the link opens the doc viewer and HTML export shows a styled page — this is the UI smoke; report if no local backend is available.

- [ ] **Step 6: Commit**

```bash
git add latte-agent-cli/ui/src/api.ts latte-agent-cli/ui/src/task_board.ts latte-agent-cli/ui/src/markdown.ts
git commit -m "feat(ui): outline link, needs_split chip and HTML export on task board"
```

---

## Task 10: Learning doc frontmatter + outline back-link — P7/P8

**Files:**
- Modify: `latte-agent-core/src/workflow.rs:9330-9396` (`finalize_learn_doc`), `latte-agent-core/src/learn_doc.rs` (parent/order helper)
- Test: `latte-agent-core` inline test for frontmatter + link insertion

- [ ] **Step 1: Write failing test**

```rust
    #[test]
    fn learn_doc_frontmatter_and_outline_link() {
        let mut outline = "<!-- node:LAT-101 -->\n### ch\n<!-- learn-links:start -->\n<!-- learn-links:end -->".to_string();
        let fm = learn_frontmatter(Some("LAT-101"), Some("LAT-100"), "docs/outlines/root.md", 2);
        assert!(fm.contains("task_id: LAT-101"));
        assert!(fm.contains("outline_node_id: LAT-101"));
        assert!(fm.contains("parent_doc: docs/learn/lat-100.md"));
        assert!(fm.contains("order: 2"));
        link_outline(&mut outline, "ch", "docs/learn/ch.md", "LAT-101").unwrap();
        assert!(outline.contains("[ch](../learn/ch.md)"), "回写 learn-links 区块: {outline}");
        assert_eq!(outline.matches("learn-links:start").count(), 1, "不能重复插区块");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p latte-agent-core learn_doc_frontmatter_and_outline_link 2>&1 | tail -40`
Expected: compile error — helpers missing.

- [ ] **Step 3: Implement helpers + wire finalize**

In `learn_doc.rs`:

```rust
/// 学习文档的归属 frontmatter（归属靠字段，不靠 slug 前缀）。
/// `task_id`/`parent_task_id` 为 None 时写 `null`（独立 /learn_loop 无看板任务）。
pub fn learn_frontmatter(
    task_id: Option<&str>,
    parent_task_id: Option<&str>,
    outline: &str,
    order: i64,
) -> String {
    let tid = task_id.unwrap_or("null");
    let parent = parent_task_id
        .map(|p| format!("docs/learn/{}.md", publish_name(&p.to_ascii_lowercase())))
        .unwrap_or_else(|| "null".to_string());
    format!("---\ntask_id: {tid}\noutline_node_id: {tid}\nparent_doc: {parent}\norder: {order}\noutline: {outline}\n---\n\n")
}
```

Corresponding test call: `let fm = learn_frontmatter(Some("LAT-101"), Some("LAT-100"), "docs/outlines/root.md", 2);`

In `workflow.rs::finalize_learn_doc`, after `let name = publish_name(slug);` (line 9336), derive the task id **from the slug prefix** — this is exactly the `lat-<digits>-` fragment that `publish_name` strips, so no new parameter is needed:

```rust
    // slug 形如 lat-110-topic：publish_name 剥掉的前缀正是看板任务 id 的小写形式。
    let task_id = {
        let s = slug.trim();
        let letters = s.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
        let digits = s[letters + 1..].bytes().take_while(|b| b.is_ascii_digit()).count();
        if letters > 0 && s.as_bytes().get(letters) == Some(&b'-') && digits > 0 {
            Some(s[..letters + 1 + digits].to_ascii_uppercase())
        } else {
            None
        }
    };
    let (parent_task_id, order) = match &task_id {
        Some(id) => read_parent_and_order(cwd, id), // 读 .latte/tasks/<id>.json
        None => (None, 0),
    };
```

`read_parent_and_order(cwd, id) -> (Option<String>, i64)` reads `.latte/tasks/<id>.json` and returns `(parent_id, sub_order)`, defaulting to `(None, 0)` when the file is absent (a standalone `/learn_loop` run has no board task — frontmatter then carries `task_id: null`, which the parser treats as "not board-linked"). Prepend `learn_frontmatter(...)` to `out` before `std::fs::write` at line 9369. Add a `link_outline(outline_path, title, doc_path, task_id)` helper in `outline.rs` that parses the doc, appends/replaces the `learn-links` entry for that node (dedupe by doc path), renders back, and writes atomically; call it from `finalize_learn_doc` after a successful publish. Generate `docs/learn/<大任务>/index.md` listing child docs in `order` — reuse `renderMarkdown` conventions but write MD, not HTML.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p latte-agent-core learn_doc_frontmatter_and_outline_link 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add latte-agent-core/src/workflow.rs latte-agent-core/src/learn_doc.rs latte-agent-core/src/outline.rs
git commit -m "feat(learn): publish frontmatter and back-link learning docs to outline tree"
```

---

## Task 11: Docs update + end-to-end smoke — P1–P10

**Files:**
- Modify: `docs/task-board-design.md`, `docs/learn-loop-doc-driven-design.md`

- [ ] **Step 1: Update task-board design doc**

In `docs/task-board-design.md:133-168`, add to the Task JSON example:

```json
  "needs_split": true,
  "outline_path": "docs/outlines/root.md",
  "doc_path": null,
```

Add a short paragraph: "`needs_split` 是规划字段（true=需继续拆），执行状态仍由 `state` 表达；`outline_path` 仅根任务落盘，子孙经祖先链解析出 `effective_outline_path`；看板任务抽屉可打开大纲并导出只读 HTML。"

- [ ] **Step 2: Update learn-loop design doc**

In `docs/learn-loop-doc-driven-design.md`, add a note near the stale §5 marker: "学习文档发布时写入 `task_id`/`outline_node_id`/`parent_doc`/`order` frontmatter，并回写大纲 `learn-links` 区块；文档树随任务树形成，详见 `docs/superpowers/specs/2026-11-17-outline-driven-workflows-design.md` §7.4。"

- [ ] **Step 3: End-to-end smoke (real flows, not tests)**

1. Start the UI server with a temp project dir. Run `/outline-split` on a sample big task → confirm board shows task tree + `🪓 待拆` chips, root has "打开大纲"; open it in the doc viewer; export HTML and confirm a readable styled page.
2. On a `needs_split=true` task, click refine → confirm children appear on board AND new `<!-- node:LAT-… -->` anchors appear under the parent in `docs/outlines/*.md` with no leftover `tmp:`.
3. Dispatch a learning child task through `learn_loop` → confirm `docs/learn/<slug>.md` has frontmatter and the outline node's `learn-links` block gained the doc link.
4. Hand-edit an outline title → `POST /api/outlines/:task_id/sync` → confirm board title changed but `state`/`runs` unchanged.
5. Force a delete-vs-modify conflict → confirm deletion wins and body landed in `docs/outlines/.trash/`.

Report which of the five were actually exercised; if a flow cannot run locally, state the exact blocker and verify the equivalent via unit test.

- [ ] **Step 4: Final commit**

```bash
git add docs/task-board-design.md docs/learn-loop-doc-driven-design.md
git commit -m "docs: update board and learn docs for outline-driven workflows"
```

---

## Self-Review Checklist (run before execution)

- [ ] Spec §2.5 P1–P10 each map to a task: P1→T6, P2→T1/T3/T6/T7, P3→T1/T6, P4→T9, P5→T7, P6→T8, P7→T10, P8→T10, P9→T9, P10→T4/T6. No gap.
- [ ] No `TBD`/`TODO`/`similar to`/placeholder steps — every code step has literal code.
- [ ] Type names consistent: `OutlineNode`, `OutlineDoc` (with `preamble`), `parse_outline`, `render_outline`, `rebind_tmp_ids`, `insert_child`, `link_outline`, `sync_with_tasks`, `check_outline_against_plan`, `require_outline_against_plan`, `effective_outline_path`, `needs_split`, `outline_path`, `doc_path`, `outline_draft_path`, `OutlineDraftRegistry`, `check_refine_allowed`, `learn_frontmatter(Option<&str>, Option<&str>, &str, i64)`, `read_parent_and_order` — used identically across tasks.
- [ ] Struct-literal compile breaks are covered: `Task` (`tasks.rs:685`, Task 1 Step 4b), `TaskView` (`tasks.rs:955`, Task 2 Step 3), `PlanTask` (5 sites, Task 3 Step 3b).
- [ ] Config-key ordering respected: `WorkflowStepDef` is `deny_unknown_fields`, so `require_outline_against_plan` is added to the Rust struct before the TOML uses it (Task 6 Step 3 → Step 5).
- [ ] Dependency order: Task 1 → 2 → 3 → 4 are independent; Task 5 (role) needs nothing; Tasks 6/7 need 1–4; Task 9 needs 1–2; Task 10 needs 4.
