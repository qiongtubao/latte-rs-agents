//! 行号自愈的 **agent 侧快照台账**（对应 oh-my-pi 的 `SnapshotStore`）。
//!
//! ## 为什么必须在 agent 层
//!
//! 工具层每次调用是无状态的：它只看到「当前磁盘内容」+「模型传来的参数」。
//! 如果模型不主动申报「我以为那几行是什么」，工具就无从判断行号准不准——
//! 于是保护退化成「靠模型自觉」，而模型恰恰是不可靠的一方。
//!
//! 真正的自愈需要一条工具拿不到的信息：**这个 agent 之前读到的是什么**。
//! 那是 session 状态，只有 agent 有。本模块就是这条台账：
//!
//! ```text
//! read  →  记录 path → (看到的内容, tag)
//! edit  →  派发前用台账自动补上 expect / tag（模型无需传任何东西）
//!          ↓
//!        工具层拿 expect 做原子校验：对得上就改，行号漂了就唯一重定位，
//!        定位不了就拒绝——绝不静默改错位置。
//! ```
//!
//! 两层各司其职、且**都不依赖模型配合**：
//! - agent 知道「模型以为的样子」（本模块）；
//! - 工具持有「落笔那一刻的真实文件」，校验必须与写入原子发生（否则
//!   agent 先查后调会有 TOCTOU 竞态）。
//!
//! ## 语义要点
//!
//! - 台账里存的是**模型最后一次「看到」的内容**（来自 read），不是磁盘现状。
//!   `expect` 由它推导 = 「模型以为那几行是什么」，这正是校验该用的基准。
//! - agent 自己的 `edit` 成功后**只刷新 tag、不动内容**：这样 tag 校验只拦
//!   「文件被外部改动」（它的本职），不会因为 agent 自己的编辑而误报；而内容
//!   仍代表模型的旧认知，行号被自己的编辑挤动后由工具层重定位自愈。
//! - `write` 整体覆盖文件后台账作废（内容已不可信）。
//!
//! ## 为什么用进程内全局表而不是穿参
//!
//! 派发路径有两处（`run_one_tool_call` 与 `run_turn` 内联），且 `HookChain`
//! 默认是 `empty()`（hook 仅 `--debug-hooks` 时注册），因此做成 hook 会变成
//! opt-in——安全机制不能可选。用按**绝对路径**键入的进程内表可以做到
//! always-on 且零签名改动；并发场景下的最终正确性由工具层的原子校验兜底。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use serde_json::Value;

/// 台账容量上限，超出按插入顺序淘汰最旧的（长会话防无界增长）。
const MAX_ENTRIES: usize = 256;

/// 某个文件「模型最后看到的样子」。
#[derive(Debug, Clone, Default)]
struct Snapshot {
    /// 全文（无选择器或 `:raw` 读取时可得）。
    full: Option<String>,
    /// 已知区段：`(起始行号 1-based, 该区段逐行内容)`。
    /// 带行范围的 read（`path:120-160`）落在这里。
    regions: Vec<(usize, Vec<String>)>,
    /// 该文件在被观测时刻的内容 tag（4 位十六进制，见工具层 `content_tag`）。
    tag: Option<String>,
}

impl Snapshot {
    /// 取 `start..=end`（1-based，含端点）的内容；覆盖不到则 `None`。
    fn slice(&self, start: usize, end: usize) -> Option<Vec<String>> {
        if start == 0 || end < start {
            return None;
        }
        if let Some(full) = &self.full {
            let lines: Vec<&str> = full.split('\n').collect();
            // 末尾换行产生的空串不计入行数。
            let lines = if lines.len() > 1 && lines.last() == Some(&"") {
                &lines[..lines.len() - 1]
            } else {
                &lines[..]
            };
            if end <= lines.len() {
                return Some(lines[start - 1..end].iter().map(|s| s.to_string()).collect());
            }
            return None;
        }
        for (region_start, region_lines) in &self.regions {
            let region_end = region_start + region_lines.len() - 1;
            if start >= *region_start && end <= region_end {
                let from = start - region_start;
                let to = end - region_start;
                return Some(region_lines[from..=to].to_vec());
            }
        }
        None
    }
}

#[derive(Default)]
struct Ledger {
    map: HashMap<String, Snapshot>,
    order: Vec<String>,
}

impl Ledger {
    fn upsert(&mut self, path: String, mutate: impl FnOnce(&mut Snapshot)) {
        if !self.map.contains_key(&path) {
            if self.order.len() >= MAX_ENTRIES {
                if let Some(oldest) = self.order.first().cloned() {
                    self.order.remove(0);
                    self.map.remove(&oldest);
                }
            }
            self.order.push(path.clone());
        }
        mutate(self.map.entry(path).or_default());
    }

    fn forget(&mut self, path: &str) {
        self.map.remove(path);
        self.order.retain(|p| p != path);
    }
}

fn ledger() -> &'static RwLock<Ledger> {
    static LEDGER: OnceLock<RwLock<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(|| RwLock::new(Ledger::default()))
}

/// 把 read 结果里的「全文 / 区段 + tag」记进台账；edit 成功后只刷新 tag；
/// write 成功后作废该文件的台账。
///
/// `result_json` 是工具返回值的 JSON 文本（派发路径上已有的序列化结果）。
/// 解析失败或形状不符一律静默忽略——台账是增益，不该让主流程失败。
pub fn record_tool_result(tool_name: &str, result_json: &str) {
    let parsed: Value = match serde_json::from_str(result_json) {
        Ok(v) => v,
        Err(_) => return,
    };

    // ── 批量 read 的 fan-out ────────────────────────────────────────
    //
    // `read` 支持 `paths` 数组批量读，返回 `{"files":[…单文件原样…]}`。
    // 这种形状顶层没有 `path`，下面的单文件分支会直接 return——台账**一条
    // 都不记**。后果不是慢，是后续 `edit` 的行号自愈失效：`heal_edit_input`
    // 找不到快照就不补 `expect`，行号一漂工具层就拒绝写入，等于批量读之后
    // 编辑能力降级。所以这里必须逐条展开。
    //
    // 批量里每个元素与单文件返回逐字段一致（同一个 `read_one_spec` 产出），
    // 所以递归调用自己即可，不需要任何形状转换。
    if tool_name == "read" {
        if let Some(files) = parsed.get("files").and_then(|v| v.as_array()) {
            for f in files {
                if let Ok(raw) = serde_json::to_string(f) {
                    record_tool_result("read", &raw);
                }
            }
            return;
        }
    }

    let path = match parsed.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return,
    };
    let tag = parsed.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());

    match tool_name {
        "read" => {
            let content = parsed.get("content").and_then(|v| v.as_str()).unwrap_or_default();
            // 折叠视图（结构摘要 / 文档大纲）的 content 不是原文，不能当内容基准；
            // 但它的 tag 仍可用于「文件是否被外部改动」的判定。
            let folded = parsed.get("mode").is_some();
            let start_line = parsed.get("startLine").and_then(|v| v.as_u64()).map(|n| n as usize);
            if let Ok(mut lg) = ledger().write() {
                lg.upsert(path, |snap| {
                    snap.tag = tag.clone().or(snap.tag.take());
                    if folded || content.is_empty() {
                        return;
                    }
                    match start_line {
                        // 带行范围的读取 → 记为区段。
                        Some(s) => {
                            let lines: Vec<String> =
                                content.split('\n').map(|s| s.to_string()).collect();
                            snap.regions.retain(|(rs, rl)| {
                                // 覆盖同一段的旧记录去掉，保留最新观测。
                                !(*rs == s && rl.len() == lines.len())
                            });
                            snap.regions.push((s, lines));
                            // 区段数量设个上限，避免长会话里无界堆积。
                            let len = snap.regions.len();
                            if len > 32 {
                                snap.regions.drain(..len - 32);
                            }
                        }
                        // 无行范围 → 全文。
                        None => {
                            snap.full = Some(content.to_string());
                            snap.regions.clear();
                        }
                    }
                });
            }
        }
        "edit" => {
            // 只刷新 tag：内容仍代表模型的旧认知（见模块文档「语义要点」）。
            if tag.is_none() {
                return;
            }
            if let Ok(mut lg) = ledger().write() {
                if lg.map.contains_key(&path) {
                    lg.upsert(path, |snap| snap.tag = tag);
                }
            }
        }
        "write" => {
            // 整体覆盖 → 旧内容全部失效。
            if let Ok(mut lg) = ledger().write() {
                lg.forget(&path);
            }
        }
        _ => {}
    }
}

/// 在 `edit` 派发前，用台账自动补上 `expect` / `tag`。
///
/// 返回补写说明（供 trace/日志）。**模型无需传任何额外参数**——这正是
/// 「程序来办、不依赖模型自觉」的落点。已由模型显式给出的 `expect`/`tag`
/// 一律尊重，不覆盖。
pub fn heal_edit_input(tool_name: &str, input: &mut Value) -> Vec<String> {
    let mut notes = Vec::new();
    if tool_name != "edit" {
        return notes;
    }
    let path = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return notes,
    };
    let snap = match ledger().read().ok().and_then(|lg| lg.map.get(&path).cloned()) {
        Some(s) => s,
        // 没读过这个文件 → 无从推导，保持原样（退回历史行为）。
        None => return notes,
    };

    // 补 tag：只拦「文件被外部改动」。模型自己显式给了就不动。
    if input.get("tag").is_none() {
        if let Some(tag) = &snap.tag {
            input["tag"] = Value::String(tag.clone());
            notes.push(format!("anchor: 自动附加 tag #{tag}（来自最近一次 read）"));
        }
    }

    let Some(ops) = input.get_mut("ops").and_then(|v| v.as_array_mut()) else {
        return notes;
    };
    for (idx, op) in ops.iter_mut().enumerate() {
        // 只给「行号操作且模型没自己给 expect」的补。
        if op.get("expect").is_some() || op.get("start_line").is_none() {
            continue;
        }
        let Some(start) = op.get("start_line").and_then(|v| v.as_u64()).map(|n| n as usize) else {
            continue;
        };
        // 插入类操作的锚点是单行；替换/删除用 end_line（缺省 = start）。
        let is_insert = op.get("insert_after").and_then(|v| v.as_bool()).unwrap_or(false)
            || op.get("insert_before").and_then(|v| v.as_bool()).unwrap_or(false);
        let end = if is_insert {
            start
        } else {
            op.get("end_line").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(start)
        };
        if let Some(lines) = snap.slice(start, end) {
            let expect = lines.join("\n");
            if let Some(obj) = op.as_object_mut() {
                obj.insert("expect".into(), Value::String(expect));
                notes.push(format!(
                    "anchor: ops[{idx}] 自动附加 expect（{start}-{end} 行，据最近一次 read）"
                ));
            }
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 台账是进程内全局表，而单测在同进程内并行跑：**每个用例必须用互不
    // 相同的路径**做隔离（不要加全局 reset——那会把并行兄弟用例的记录
    // 一起清掉，造成偶发失败）。

    /// 批量 read（`{"files":[…]}`）必须逐条进台账，且随后的 `edit` 行号
    /// 自愈照常工作。
    ///
    /// 这是本次批量读改造里**唯一会真出 bug 的地方**：`record_tool_result`
    /// 原来只认扁平单文件形状，批量形状顶层没有 `path` 会被直接 return，
    /// 台账一条都不记 → `heal_edit_input` 补不出 `expect` → 行号一漂工具层
    /// 就拒绝写入。症状不是慢，是批量读之后编辑能力静默降级。
    #[test]
    fn batch_read_fans_out_into_ledger_and_edit_still_heals() {
        let batch = json!({
            "files": [
                {"path":"/tmp/batch1.rs","content":"a1\na2\na3\n","tag":"T001"},
                {"path":"/tmp/batch2.rs","content":"b1\nb2\n","tag":"T002"},
                {"path":"/tmp/batch3.rs","content":"c1\nc2\nc3\nc4\n","tag":"T003"},
                {"path":"/tmp/batch4.rs","content":"d1\n","tag":"T004"},
                // 折叠视图：只记 tag、不记内容（与单文件语义一致）。
                {"path":"/tmp/batch5.rs","content":"fn x() …elided","mode":"structural_summary","tag":"T005"},
            ],
            "failed": [{"path":"/tmp/batch_missing.rs","error":"stat: no such file"}],
            "count": 5,
            "failedCount": 1,
        });
        record_tool_result("read", &batch.to_string());

        // 5 条快照都在，且 tag 各自正确。
        for (i, tag) in ["T001", "T002", "T003", "T004", "T005"].iter().enumerate() {
            let p = format!("/tmp/batch{}.rs", i + 1);
            let snap = ledger()
                .read()
                .ok()
                .and_then(|lg| lg.map.get(&p).cloned())
                .unwrap_or_else(|| panic!("{p} 应有台账快照"));
            assert_eq!(snap.tag.as_deref(), Some(*tag), "{p} 的 tag");
        }

        // 非折叠项：expect + tag 都能自愈。
        let mut input =
            json!({"path":"/tmp/batch3.rs","ops":[{"start_line":2,"end_line":2,"new_content":"X"}]});
        let notes = heal_edit_input("edit", &mut input);
        assert_eq!(input["tag"], "T003");
        assert_eq!(input["ops"][0]["expect"], "c2");
        assert_eq!(notes.len(), 2, "两条补写说明: {notes:?}");

        // 折叠项：只补 tag，不猜内容。
        let mut folded = json!({"path":"/tmp/batch5.rs","ops":[{"start_line":1,"new_content":"X"}]});
        heal_edit_input("edit", &mut folded);
        assert_eq!(folded["tag"], "T005");
        assert!(
            folded["ops"][0].get("expect").is_none(),
            "折叠视图的 content 不是原文，不能当 expect 基准"
        );

        // 失败项不该进台账（没读到就是没读到）。
        let missing = ledger()
            .read()
            .ok()
            .and_then(|lg| lg.map.get("/tmp/batch_missing.rs").cloned());
        assert!(missing.is_none(), "failed 里的路径不该有快照");
    }

    #[test]
    fn full_read_then_edit_gets_expect_and_tag() {
        record_tool_result(
            "read",
            &json!({"path":"/tmp/a.rs","content":"l1\nl2\nl3\n","tag":"AB12"}).to_string(),
        );
        let mut input = json!({"path":"/tmp/a.rs","ops":[{"start_line":2,"end_line":2,"new_content":"X"}]});
        let notes = heal_edit_input("edit", &mut input);
        assert_eq!(input["tag"], "AB12", "应自动附加 tag");
        assert_eq!(input["ops"][0]["expect"], "l2", "应自动附加 expect");
        assert_eq!(notes.len(), 2, "两条补写说明: {notes:?}");
    }

    #[test]
    fn ranged_read_covers_only_that_region() {
        record_tool_result(
            "read",
            &json!({"path":"/tmp/b.rs","content":"aa\nbb","startLine":10,"endLine":11,"tag":"CD34"})
                .to_string(),
        );
        // 命中区段内。
        let mut hit = json!({"path":"/tmp/b.rs","ops":[{"start_line":11,"new_content":"X"}]});
        heal_edit_input("edit", &mut hit);
        assert_eq!(hit["ops"][0]["expect"], "bb");
        // 区段外 → 不补 expect（无从推导，不猜）。
        let mut miss = json!({"path":"/tmp/b.rs","ops":[{"start_line":50,"new_content":"X"}]});
        heal_edit_input("edit", &mut miss);
        assert!(miss["ops"][0].get("expect").is_none(), "区段外不应补 expect");
    }

    #[test]
    fn folded_summary_read_records_tag_but_not_content() {
        record_tool_result(
            "read",
            &json!({"path":"/tmp/c.rs","content":"fn a() …elided","mode":"structural_summary","tag":"EF56"})
                .to_string(),
        );
        let mut input = json!({"path":"/tmp/c.rs","ops":[{"start_line":1,"new_content":"X"}]});
        heal_edit_input("edit", &mut input);
        assert_eq!(input["tag"], "EF56", "折叠视图的 tag 仍可用");
        assert!(
            input["ops"][0].get("expect").is_none(),
            "折叠内容不是原文，不能当 expect 基准"
        );
    }

    #[test]
    fn explicit_model_values_are_not_overwritten() {
        record_tool_result(
            "read",
            &json!({"path":"/tmp/d.rs","content":"x\ny\n","tag":"1111"}).to_string(),
        );
        let mut input = json!({
            "path":"/tmp/d.rs","tag":"9999",
            "ops":[{"start_line":1,"expect":"model_said","new_content":"X"}]
        });
        heal_edit_input("edit", &mut input);
        assert_eq!(input["tag"], "9999", "模型显式给的 tag 不覆盖");
        assert_eq!(input["ops"][0]["expect"], "model_said", "模型显式给的 expect 不覆盖");
    }

    #[test]
    fn edit_refreshes_tag_only_and_keeps_content_belief() {
        record_tool_result(
            "read",
            &json!({"path":"/tmp/e.rs","content":"p\nq\n","tag":"AAAA"}).to_string(),
        );
        // agent 自己编辑后，tag 变了但内容认知保留。
        record_tool_result("edit", &json!({"path":"/tmp/e.rs","tag":"BBBB"}).to_string());
        let mut input = json!({"path":"/tmp/e.rs","ops":[{"start_line":2,"new_content":"Z"}]});
        heal_edit_input("edit", &mut input);
        assert_eq!(input["tag"], "BBBB", "tag 应刷新为编辑后的新值（不误报 stale）");
        assert_eq!(input["ops"][0]["expect"], "q", "内容认知保留，供工具层重定位");
    }

    #[test]
    fn write_invalidates_ledger() {
        record_tool_result(
            "read",
            &json!({"path":"/tmp/f.rs","content":"old\n","tag":"AAAA"}).to_string(),
        );
        record_tool_result("write", &json!({"path":"/tmp/f.rs"}).to_string());
        let mut input = json!({"path":"/tmp/f.rs","ops":[{"start_line":1,"new_content":"X"}]});
        let notes = heal_edit_input("edit", &mut input);
        assert!(input.get("tag").is_none(), "write 后台账作废，不应再补 tag");
        assert!(input["ops"][0].get("expect").is_none(), "write 后不应再补 expect");
        assert!(notes.is_empty());
    }

    #[test]
    fn unknown_file_and_non_edit_tools_are_untouched() {
        let mut input = json!({"path":"/tmp/never_read.rs","ops":[{"start_line":1,"new_content":"X"}]});
        assert!(heal_edit_input("edit", &mut input).is_empty(), "没读过 → 保持原样");
        assert!(input.get("tag").is_none());
        let mut other = json!({"path":"/tmp/a.rs"});
        assert!(heal_edit_input("search", &mut other).is_empty(), "非 edit 工具不处理");
    }

    #[test]
    fn insert_op_anchors_on_single_line() {
        record_tool_result(
            "read",
            &json!({"path":"/tmp/g.rs","content":"one\ntwo\nthree\n","tag":"7777"}).to_string(),
        );
        let mut input = json!({
            "path":"/tmp/g.rs",
            "ops":[{"start_line":2,"end_line":99,"insert_after":true,"new_content":"X"}]
        });
        heal_edit_input("edit", &mut input);
        assert_eq!(
            input["ops"][0]["expect"], "two",
            "插入锚点是单行，忽略无意义的 end_line"
        );
    }

    /// 端到端：真实 `read` → 文件在别处被插入两行（行号漂移）→ 模型仍按
    /// 旧行号发 `edit`，台账自动补 expect，工具层唯一重定位，**改中真正
    /// 想改的那一行**。全程模型未传 expect/tag，纯程序自愈。
    ///
    /// 这条用例是本机制的核心主张：修复前同样的调用会静默改错行。
    #[tokio::test]
    async fn end_to_end_line_drift_self_heals() {
        use latte_rs_agent_tools::core::create_tool_manager;
        use latte_rs_agent_tools::types::ToolManager;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("drift.txt");
        std::fs::write(&file, "a\nb\nTARGET\nd\n").unwrap();
        let path = file.to_string_lossy().to_string();

        let tm = create_tool_manager();
        tm.register_package(latte_rs_agent_tools::tools::FileToolsPackage::new())
            .await
            .unwrap();
        tm.register_package(latte_rs_agent_tools::tools::EditToolsPackage::new())
            .await
            .unwrap();

        // 1. 真实 read —— 模型看到 TARGET 在第 3 行。
        let read_out = tm
            .execute("read", json!({"path": path}), None)
            .await
            .expect("read 应成功");
        record_tool_result("read", &serde_json::to_string(&read_out).unwrap());

        // 2. 文件在前面被插入两行 → TARGET 漂到第 5 行（模型并不知道）。
        std::fs::write(&file, "new1\nnew2\na\nb\nTARGET\nd\n").unwrap();

        // 3. 模型按**旧**行号 3 发 edit，且完全没传 expect / tag。
        let mut input = json!({
            "path": path,
            "ops": [{"start_line": 3, "end_line": 3, "new_content": "HEALED"}]
        });
        let notes = heal_edit_input("edit", &mut input);
        assert!(!notes.is_empty(), "台账应补上 expect/tag: {notes:?}");
        assert_eq!(input["ops"][0]["expect"], "TARGET", "expect 应取自 read 时的第 3 行");

        // 4. 派发给真实 edit 工具：行号对不上 → 唯一重定位到第 5 行。
        let edit_out = tm
            .execute("edit", input, None)
            .await
            .expect("自愈后编辑应成功");
        let warnings = edit_out["warnings"].as_array().expect("应有自愈 warning");
        assert!(
            warnings.iter().any(|w| w.as_str().unwrap_or("").contains("行号自愈")),
            "应报告行号重定位: {warnings:?}"
        );

        // 5. 断言改中的是 TARGET 那一行，而不是旧行号 3（那里现在是 "a"）。
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            after, "new1\nnew2\na\nb\nHEALED\nd\n",
            "必须改中 TARGET 所在行；若改到第 3 行则是静默改错（回归）"
        );
    }

    /// 端到端反面：expect 所指内容已不存在（模型认知彻底过期）→ 工具拒绝，
    /// 文件保持原样，而不是按错行号乱改。
    #[tokio::test]
    async fn end_to_end_unrelocatable_edit_is_refused() {
        use latte_rs_agent_tools::core::create_tool_manager;
        use latte_rs_agent_tools::types::ToolManager;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gone.txt");
        std::fs::write(&file, "x\nOLD_LINE\nz\n").unwrap();
        let path = file.to_string_lossy().to_string();

        let tm = create_tool_manager();
        tm.register_package(latte_rs_agent_tools::tools::FileToolsPackage::new())
            .await
            .unwrap();
        tm.register_package(latte_rs_agent_tools::tools::EditToolsPackage::new())
            .await
            .unwrap();

        let read_out = tm.execute("read", json!({"path": path}), None).await.unwrap();
        record_tool_result("read", &serde_json::to_string(&read_out).unwrap());

        // OLD_LINE 被彻底删掉 → 无法重定位。
        let replaced = "x\nSOMETHING_ELSE\nz\n";
        std::fs::write(&file, replaced).unwrap();

        let mut input = json!({
            "path": path,
            "ops": [{"start_line": 2, "end_line": 2, "new_content": "NEW"}]
        });
        heal_edit_input("edit", &mut input);
        let err = tm.execute("edit", input, None).await.expect_err("应拒绝");
        assert!(
            err.to_string().contains("stale tag") || err.to_string().contains("expect"),
            "应因 stale tag 或 expect 不符而拒绝: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            replaced,
            "拒绝时不得改动文件"
        );
    }
}
