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

/// 保留的条目数上限，超出按 **LRU** 淘汰（最久未被使用的先走）。
///
/// 原先是「按插入顺序淘汰最旧的」= FIFO。区别在长会话里很实：反复读写的
/// 热文件只要是早期首次记录的，就会被后来一堆只读一次的文件挤掉，然后它
/// 的行号自愈失效。对齐 oh-my-pi 的 `DEFAULT_MAX_PATHS`（同样 256，同样
/// LRU）。
const MAX_ENTRIES: usize = 256;

/// 全局保留快照文本的字节上限。
///
/// 原先**完全没有**字节维度的约束：只数条目数和区段数，而每条 `Snapshot`
/// 存的是文件**全文**。256 个大文件就能把内存吃到 GB 级。对齐 oh-my-pi 的
/// `DEFAULT_MAX_TOTAL_BYTES`。
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// 单文件字节门槛：超过这个大小的内容不进台账。
///
/// 超大文件多半是生成物/词表，模型也不会真去逐行引用它；把它收进来只会
/// 一次吃掉整个字节预算、把有用的快照全挤出去。对齐 oh-my-pi 的
/// `MAX_SNAPSHOT_FILE_BYTES`。
const MAX_SNAPSHOT_FILE_BYTES: usize = 4 * 1024 * 1024;

/// 台账作用域键：`session_id \0 role \0 path`。
///
/// # 为什么不能只按路径
///
/// 原先 key 就是路径，台账是**进程级全局**的。后果是同一进程里所有 agent
/// 共享观察记录：workflow 的 `survey` 步（programmer）读过某文件，
/// `breakdown` 步（architect）根本没读，但查台账查得到。对「行号自愈」来说
/// 这只是偏宽松；对「引用的东西是否真的观察过」这类判据就是直接失效。
///
/// # 为什么用 role 而不是 subagent id
///
/// 实测：`build_runner` 的所有调用点传的都是 `&config.session_id`，即
/// **父会话 id**——subagent 并没有自己的 session_id（事件层的 `sub_id`
/// 有，但没挂到 runner 上）。所以退一步用 role 作细分轴：它能把上面那个
/// programmer/architect 的例子分开，也不会把「同一角色跨轮次继续干活」
/// 误切开（那种情况本就该共享）。真正的按 subagent 隔离需要把 sub_id
/// 一路传下来，是后续的事。
///
/// 复合键用 `\0` 拼接，同 oh-my-pi 的 chain key
/// （`baseUrl \0 model.id \0 sessionId`）。
/// 台账作用域：一次观察归属于「哪个会话的哪个角色」。
///
/// 调用方（`AgentRunner`）持有 `session_id` 与 `role_id`，构造这个轻量值
/// 传进来即可；台账内部据此拼出复合键。
#[derive(Debug, Clone)]
pub struct Scope {
    session_id: String,
    role: String,
}

impl Scope {
    /// 从会话与角色构造作用域。
    pub fn new(session_id: impl Into<String>, role: impl Into<String>) -> Self {
        Self { session_id: session_id.into(), role: role.into() }
    }

    fn key(&self, path: &str) -> String {
        scope_key(&self.session_id, &self.role, path)
    }
}

fn scope_key(session_id: &str, role: &str, path: &str) -> String {
    format!("{session_id}\0{role}\0{path}")
}

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
    /// 模型**实际看到过**的行号（1-based）。
    ///
    /// 与 `full` / `regions` 是**两件事**，必须分开存：
    ///
    /// - `full` / `regions` 存的是**内容**，`heal_edit_input` 拿它给行号操作
    ///   补 `expect`。只有 `read` 能提供真实原文，所以只有 read 写这两项。
    /// - `seen_lines` 存的是**来源**（"这些行确实在模型眼前出现过"）。
    ///   `search` 只回匹配行、`code_graph` 只回签名，都拿不到连续原文——
    ///   把它们塞进 `regions` 会让 `expect` 补出**假内容**，直接写坏文件。
    ///   但它们确实构成观察，记在这里。
    ///
    /// 对齐 oh-my-pi `pi-edit` 的 `Snapshot { text, seen_lines }`——它同样
    /// 把整文件快照和"展示过哪些行"拆成两个字段。
    seen_lines: std::collections::BTreeSet<u32>,
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
    /// 同一文件**连续**锚点类 `edit` 失败次数。对该文件成功 read/edit/write
    /// 即清零。见 [`bump_anchor_failure`]。
    anchor_failures: HashMap<String, u32>,
    /// 当前保留的快照文本合计字节数（`MAX_TOTAL_BYTES` 的记账口）。
    total_bytes: usize,
}

impl Snapshot {
    /// 本条快照占用的字节数（用于全局字节预算记账）。
    fn footprint(&self) -> usize {
        self.full.as_ref().map(|f| f.len()).unwrap_or(0)
            + self
                .regions
                .iter()
                .map(|(_, lines)| lines.iter().map(|l| l.len() + 1).sum::<usize>())
                .sum::<usize>()
            // 大文件的 seen_lines 本身也能到几百 KB，必须计入预算。
            + self.seen_lines.len() * std::mem::size_of::<u32>()
    }
}

impl Ledger {
    fn upsert(&mut self, key: String, mutate: impl FnOnce(&mut Snapshot)) {
        let existed = self.map.contains_key(&key);
        if existed {
            // LRU：命中就把它挪到队尾（最近使用）。
            self.order.retain(|k| k != &key);
        } else {
            self.evict_until_entries_below(MAX_ENTRIES.saturating_sub(1));
        }
        self.order.push(key.clone());

        let before = self.map.get(&key).map(|s| s.footprint()).unwrap_or(0);
        let entry = self.map.entry(key.clone()).or_default();
        mutate(entry);
        let after = entry.footprint();
        self.total_bytes = self.total_bytes + after - before.min(self.total_bytes);

        // 字节预算：从最久未使用的开始丢，直到降到上限内。**永不丢刚写的
        // 那条**——否则一个超大文件会把自己也清掉，等于白记。
        self.evict_until_bytes_below(MAX_TOTAL_BYTES, &key);
    }

    /// 按 LRU 淘汰到条目数不超过 `limit`。
    fn evict_until_entries_below(&mut self, limit: usize) {
        while self.order.len() > limit {
            let Some(oldest) = self.order.first().cloned() else {
                break;
            };
            self.order.remove(0);
            if let Some(s) = self.map.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(s.footprint());
            }
        }
    }

    /// 按 LRU 淘汰到总字节不超过 `limit`，但跳过 `keep`。
    fn evict_until_bytes_below(&mut self, limit: usize, keep: &str) {
        while self.total_bytes > limit {
            let Some(victim) = self.order.iter().find(|k| k.as_str() != keep).cloned() else {
                break; // 只剩 keep 一条了，再丢就没意义
            };
            self.order.retain(|k| k != &victim);
            if let Some(s) = self.map.remove(&victim) {
                self.total_bytes = self.total_bytes.saturating_sub(s.footprint());
            }
        }
    }

    fn forget(&mut self, key: &str) {
        if let Some(s) = self.map.remove(key) {
            self.total_bytes = self.total_bytes.saturating_sub(s.footprint());
        }
        self.order.retain(|k| k != key);
        self.anchor_failures.remove(key);
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
pub fn record_tool_result(scope: &Scope, tool_name: &str, result_json: &str) {
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
                    record_tool_result(scope, "read", &raw);
                }
            }
            return;
        }
    }

    // ── 取证工具的来源记录 ─────────────────────────────────────────
    //
    // `search` / `code_graph` 的返回顶层**没有** `path`（一次调用横跨多个
    // 文件），会被下面那个 early return 直接丢掉——这正是原来它们完全不进
    // 台账的原因。它们只提供「哪些行在模型眼前出现过」，不提供连续原文，
    // 所以只写 `seen_lines`、不碰 `full` / `regions`（见 `Snapshot::seen_lines`
    // 的文档：塞假内容会让 `expect` 补错、写坏文件）。
    //
    // 记录方式对齐 oh-my-pi 的 `recordSeenLinesFromBody`：从**工具实际回传
    // 的内容**里反解行号，而不是让每个工具自己声明"我展示了什么"。截断、
    // 上限、折叠都已经体现在回传内容里，反解出来的必然与模型看到的一致。
    if tool_name == "search" {
        record_search_seen_lines(scope, &parsed);
        return;
    }
    if tool_name == "code_graph" {
        record_code_graph_seen_lines(scope, &parsed);
        return;
    }

    let path = match parsed.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return,
    };
    let tag = parsed.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());

    // 这个工具对该文件成功了 → 锚点失败连击断了，清零。
    // 放在 match 之前是因为三个分支（read/edit/write）都算「有了新的
    // 权威观测」，而 write 走 forget 也会顺手清掉。
    if matches!(tool_name, "read" | "edit" | "write") {
        clear_anchor_failures(&scope.key(&path));
    }

    match tool_name {
        "read" => {
            let content = parsed.get("content").and_then(|v| v.as_str()).unwrap_or_default();
            // 折叠视图（结构摘要 / 文档大纲）的 content 不是原文，不能当内容基准；
            // 但它的 tag 仍可用于「文件是否被外部改动」的判定。
            let folded = parsed.get("mode").is_some();
            let start_line = parsed.get("startLine").and_then(|v| v.as_u64()).map(|n| n as usize);
            if let Ok(mut lg) = ledger().write() {
                lg.upsert(scope.key(&path), |snap| {
                    snap.tag = tag.clone().or(snap.tag.take());
                    if folded || content.is_empty() {
                        return;
                    }
                    // 超大内容不收：多半是生成物/词表，模型也不会逐行引用它，
                    // 收进来只会一次吃光字节预算、把有用的快照全挤出去。
                    // tag 仍然留着（上面已写），所以"文件变没变"的判定不受影响。
                    if content.len() > MAX_SNAPSHOT_FILE_BYTES {
                        return;
                    }
                    match start_line {
                        // 带行范围的读取 → 记为区段。
                        Some(s) => {
                            let lines: Vec<String> =
                                content.split('\n').map(|s| s.to_string()).collect();
                            // 来源：这一段的行号都算看过。
                            for n in 0..lines.len() {
                                snap.seen_lines.insert((s + n) as u32);
                            }
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
                            let total = content.split('\n').count();
                            snap.full = Some(content.to_string());
                            snap.regions.clear();
                            for n in 1..=total {
                                snap.seen_lines.insert(n as u32);
                            }
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
                let key = scope.key(&path);
                if lg.map.contains_key(&key) {
                    lg.upsert(key, |snap| snap.tag = tag);
                }
            }
        }
        "write" => {
            // 整体覆盖 → 旧内容全部失效。
            if let Ok(mut lg) = ledger().write() {
                lg.forget(&scope.key(&path));
            }
        }
        _ => {}
    }
}

/// 同一文件连续多少次锚点类失败后升级为硬性「换策略」指令。
///
/// 抄 oh-my-pi 的 `store.record_noop`：先给软提示，达到阈值再升级。3 次是
/// 它用的阈值——两次还能算手滑，第三次就是在打转了。
const ANCHOR_FAILURE_ESCALATE_AT: u32 = 3;

/// 记一次锚点类 `edit` 失败，返回 `(该文件连续失败次数, 是否该升级)`。
///
/// **为什么不能靠现有的死循环熔断**：`AgentRunner` 的熔断按「工具名 + 逐
/// 字节相同的参数」计数（连击 5 次）或「最近 8 次里 ≤2 种调用且各自重复」
/// （打转）。实测 2026-09-10 jemalloc 会话 `ui-…-6`：programmer 对同一份
/// 笔记连续 4 次 edit 失败，但每次 `old_text` 都不一样（它在猜锚点），两条
/// 规则一条都没触发，白烧 4 轮往返。所以这里按**语义类别**计数而不是按
/// 参数指纹。
fn bump_anchor_failure(path: &str) -> (u32, bool) {
    let Ok(mut lg) = ledger().write() else {
        return (0, false);
    };
    let count = lg.anchor_failures.entry(path.to_string()).or_insert(0);
    *count += 1;
    let n = *count;
    (n, n >= ANCHOR_FAILURE_ESCALATE_AT)
}

/// 对某个文件的锚点失败计数清零（成功 read / edit / write 后调用）。
fn clear_anchor_failures(path: &str) {
    if let Ok(mut lg) = ledger().write() {
        lg.anchor_failures.remove(path);
    }
}

/// 软提示：单次锚点失败后追加。只点「重读」，不喊停。
fn soft_note(path: &str) -> String {
    format!("\n\n[锚点提示] 对 `{path}` 的这次编辑没能定位到锚点。先 `read` 它、\
             按当前内容重写 old_text，再改。")
}

/// 硬性升级：连续失败到阈值后追加，要求**换策略**而不是继续猜。
///
/// 话术照 oh-my-pi 的 `no_change_loop_diagnostic` 的骨架来：点明软提示没
/// 起效、要求停止重发、给出两条明确出路、并说明再不变还会被拒。
fn escalation_note(path: &str, count: u32) -> String {
    format!(
        "\n\n[停] 对 `{path}` 的编辑已连续 {count} 次定位不到锚点，前面的提示没有\
         打破这个循环。停止继续猜 old_text。只有两条出路：(1) 用 `read` 重新\
         读这个文件（不带行号选择器，或读到目标行所在区段），按**返回的原文**\
         逐字复制锚点、并带上返回的 tag；(2) 如果你要写的内容其实已经在文件\
         里了，就别改了，往下走。凭记忆或凭你之前的输出构造的锚点会一直被拒。"
    )
}

/// `edit` 失败后的锚点记账，返回要追加到回喂给模型的报文后面的文本。
///
/// 只对锚点类失败（`EDIT_ANCHOR_NOT_FOUND` / `EDIT_AMBIGUOUS` /
/// `EDIT_STALE_TAG`）计数——参数不合法（`EDIT_BAD_ARGS`）是另一类毛病，
/// 混在一起计数会让阈值失去含义。非 `edit` 工具、非锚点类失败一律返回
/// `None`，调用方原样回喂。
pub fn note_edit_failure(
    scope: &Scope,
    tool_name: &str,
    input: &Value,
    error: &latte_rs_agent_tools::error::ToolError,
) -> Option<String> {
    if tool_name != "edit" {
        return None;
    }
    let code = latte_rs_agent_tools::tools::edit::failure_code(error)?;
    if !matches!(
        code,
        latte_rs_agent_tools::tools::edit::EDIT_ANCHOR_NOT_FOUND
            | latte_rs_agent_tools::tools::edit::EDIT_AMBIGUOUS
            | latte_rs_agent_tools::tools::edit::EDIT_STALE_TAG
    ) {
        return None;
    }
    let path = input.get("path").and_then(|v| v.as_str())?;
    let (count, escalate) = bump_anchor_failure(&scope.key(path));
    Some(if escalate {
        escalation_note(path, count)
    } else {
        soft_note(path)
    })
}

/// 单次调用最多为一个文件记多少行来源（防超大结果把预算吃光）。
const MAX_SEEN_PER_CALL: usize = 4_000;

/// `search` 的返回：`{"matches":[{"file","line","content"}, …]}`。
fn record_search_seen_lines(scope: &Scope, parsed: &Value) {
    let Some(matches) = parsed.get("matches").and_then(|v| v.as_array()) else {
        return;
    };
    let mut by_file: HashMap<String, Vec<u32>> = HashMap::new();
    for m in matches {
        let Some(file) = m.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(line) = m.get("line").and_then(|v| v.as_u64()) else {
            continue;
        };
        if line == 0 {
            continue;
        }
        by_file.entry(file.to_string()).or_default().push(line as u32);
    }
    merge_seen(scope, by_file);
}

/// `code_graph` 的返回：`matches` 是**渲染后的整段文本**，每行形如
/// `path:12-30: fn foo(...)` 或 `path:12: …`。
fn record_code_graph_seen_lines(scope: &Scope, parsed: &Value) {
    let Some(text) = parsed.get("matches").and_then(|v| v.as_str()) else {
        return;
    };
    let mut by_file: HashMap<String, Vec<u32>> = HashMap::new();
    for line in text.lines() {
        // 从右往左找 ": "，左边是 `path:span`（路径自身可能含冒号，所以不能
        // 从左边切）。
        let Some((locator, _)) = line.split_once(": ") else {
            continue;
        };
        let Some((file, span)) = locator.rsplit_once(':') else {
            continue;
        };
        let (start, end) = match span.split_once('-') {
            Some((a, b)) => (a.parse::<u32>().ok(), b.parse::<u32>().ok()),
            None => (span.parse::<u32>().ok(), None),
        };
        let Some(start) = start.filter(|n| *n > 0) else {
            continue;
        };
        let end = end.filter(|e| *e >= start).unwrap_or(start);
        let entry = by_file.entry(file.to_string()).or_default();
        for n in start..=end {
            entry.push(n);
            if entry.len() >= MAX_SEEN_PER_CALL {
                break;
            }
        }
    }
    merge_seen(scope, by_file);
}

/// 把「文件 → 行号」并进台账的 `seen_lines`（union 语义，不覆盖既有观察）。
fn merge_seen(scope: &Scope, by_file: HashMap<String, Vec<u32>>) {
    if by_file.is_empty() {
        return;
    }
    let Ok(mut lg) = ledger().write() else {
        return;
    };
    for (file, lines) in by_file {
        lg.upsert(scope.key(&file), |snap| {
            for n in lines.into_iter().take(MAX_SEEN_PER_CALL) {
                snap.seen_lines.insert(n);
            }
        });
    }
}

/// 查询某文件在本作用域内**被观察过的行号**。
///
/// 返回 `None` 表示这个文件在台账里根本没有记录——含义是「不知道看没看过」，
/// **不是**「没看过」。这个三态是刻意的：判据的调用方遇到 `None` 应当降级
/// 放行，而不是拒绝（对齐 oh-my-pi 的 `seen_lines: Option<BTreeSet<u32>>`，
/// `None` = provenance not recorded）。
///
/// 台账有容量上限、会 LRU 淘汰，所以「查不到」既可能是没读过，也可能是读过
/// 但被挤出去了——这正是不能把 `None` 当拒绝依据的原因。
pub fn observed_lines(scope: &Scope, path: &str) -> Option<std::collections::BTreeSet<u32>> {
    let lg = ledger().read().ok()?;
    let snap = lg.map.get(&scope.key(path))?;
    if snap.seen_lines.is_empty() {
        return None;
    }
    Some(snap.seen_lines.clone())
}

/// 按**引用里写的路径**在本作用域找观察记录。
///
/// 产出里的引用常是 basename 或短相对路径（实测 jemalloc 文档写
/// `` `bit_util.h:343` ``，而台账里的键是
/// `include/jemalloc/internal/bit_util.h`），所以要做后缀匹配。
///
/// 返回 `None` 的三种情形都表示「**不能据此判定**」，调用方应放行：
/// - 台账里没有任何路径以它结尾 → 没观察过、或观察过但已被 LRU 淘汰；
/// - 命中多个路径（同名文件在不同目录）→ 无法确定说的是哪个；
/// - 命中的那条没有行级来源（只有 tag，比如折叠视图）。
pub fn observed_lines_for_cited_path(
    scope: &Scope,
    cited: &str,
) -> Option<(String, std::collections::BTreeSet<u32>)> {
    let cited = cited.trim().trim_start_matches("./");
    if cited.is_empty() {
        return None;
    }
    let lg = ledger().read().ok()?;
    let prefix = scope.key("");
    let mut hit: Option<(String, &Snapshot)> = None;
    for (key, snap) in lg.map.iter() {
        // 只看本作用域的条目。
        let Some(path) = key.strip_prefix(&prefix) else {
            continue;
        };
        let matches = path == cited
            || path.ends_with(&format!("/{cited}"))
            || path.ends_with(&format!("\\{cited}"));
        if !matches {
            continue;
        }
        if hit.is_some() {
            return None; // 同名歧义 → 不判
        }
        hit = Some((path.to_string(), snap));
    }
    let (path, snap) = hit?;
    if snap.seen_lines.is_empty() {
        return None; // 只有 tag、没有行级来源
    }
    Some((path, snap.seen_lines.clone()))
}

/// 在 `edit` 派发前，用台账自动补上 `expect` / `tag`。
///
/// 返回补写说明（供 trace/日志）。**模型无需传任何额外参数**——这正是
/// 「程序来办、不依赖模型自觉」的落点。已由模型显式给出的 `expect`/`tag`
/// 一律尊重，不覆盖。
pub fn heal_edit_input(scope: &Scope, tool_name: &str, input: &mut Value) -> Vec<String> {
    let mut notes = Vec::new();
    if tool_name != "edit" {
        return notes;
    }
    let path = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return notes,
    };
    let snap = match ledger()
        .read()
        .ok()
        .and_then(|lg| lg.map.get(&scope.key(&path)).cloned())
    {
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

    /// 每个用例自己的作用域。台账现在按 `session\0role\0path` 键，所以
    /// 用例之间只要 session 名不同就天然隔离——比原来靠"路径各不相同"更稳。
    fn sc(name: &str) -> Scope {
        Scope::new(name, "tester")
    }

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
        record_tool_result(&sc("t"), "read", &batch.to_string());

        // 5 条快照都在，且 tag 各自正确。
        for (i, tag) in ["T001", "T002", "T003", "T004", "T005"].iter().enumerate() {
            let p = format!("/tmp/batch{}.rs", i + 1);
            let snap = ledger()
                .read()
                .ok()
                .and_then(|lg| lg.map.get(&sc("t").key(&p)).cloned())
                .unwrap_or_else(|| panic!("{p} 应有台账快照"));
            assert_eq!(snap.tag.as_deref(), Some(*tag), "{p} 的 tag");
        }

        // 非折叠项：expect + tag 都能自愈。
        let mut input =
            json!({"path":"/tmp/batch3.rs","ops":[{"start_line":2,"end_line":2,"new_content":"X"}]});
        let notes = heal_edit_input(&sc("t"), "edit", &mut input);
        assert_eq!(input["tag"], "T003");
        assert_eq!(input["ops"][0]["expect"], "c2");
        assert_eq!(notes.len(), 2, "两条补写说明: {notes:?}");

        // 折叠项：只补 tag，不猜内容。
        let mut folded = json!({"path":"/tmp/batch5.rs","ops":[{"start_line":1,"new_content":"X"}]});
        heal_edit_input(&sc("t"), "edit", &mut folded);
        assert_eq!(folded["tag"], "T005");
        assert!(
            folded["ops"][0].get("expect").is_none(),
            "折叠视图的 content 不是原文，不能当 expect 基准"
        );

        // 失败项不该进台账（没读到就是没读到）。
        let missing = ledger()
            .read()
            .ok()
            .and_then(|lg| lg.map.get(&sc("t").key("/tmp/batch_missing.rs")).cloned());
        assert!(missing.is_none(), "failed 里的路径不该有快照");
    }

    #[test]
    fn full_read_then_edit_gets_expect_and_tag() {
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path":"/tmp/a.rs","content":"l1\nl2\nl3\n","tag":"AB12"}).to_string(),
        );
        let mut input = json!({"path":"/tmp/a.rs","ops":[{"start_line":2,"end_line":2,"new_content":"X"}]});
        let notes = heal_edit_input(&sc("t"), "edit", &mut input);
        assert_eq!(input["tag"], "AB12", "应自动附加 tag");
        assert_eq!(input["ops"][0]["expect"], "l2", "应自动附加 expect");
        assert_eq!(notes.len(), 2, "两条补写说明: {notes:?}");
    }

    #[test]
    fn ranged_read_covers_only_that_region() {
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path":"/tmp/b.rs","content":"aa\nbb","startLine":10,"endLine":11,"tag":"CD34"})
                .to_string(),
        );
        // 命中区段内。
        let mut hit = json!({"path":"/tmp/b.rs","ops":[{"start_line":11,"new_content":"X"}]});
        heal_edit_input(&sc("t"), "edit", &mut hit);
        assert_eq!(hit["ops"][0]["expect"], "bb");
        // 区段外 → 不补 expect（无从推导，不猜）。
        let mut miss = json!({"path":"/tmp/b.rs","ops":[{"start_line":50,"new_content":"X"}]});
        heal_edit_input(&sc("t"), "edit", &mut miss);
        assert!(miss["ops"][0].get("expect").is_none(), "区段外不应补 expect");
    }

    #[test]
    fn folded_summary_read_records_tag_but_not_content() {
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path":"/tmp/c.rs","content":"fn a() …elided","mode":"structural_summary","tag":"EF56"})
                .to_string(),
        );
        let mut input = json!({"path":"/tmp/c.rs","ops":[{"start_line":1,"new_content":"X"}]});
        heal_edit_input(&sc("t"), "edit", &mut input);
        assert_eq!(input["tag"], "EF56", "折叠视图的 tag 仍可用");
        assert!(
            input["ops"][0].get("expect").is_none(),
            "折叠内容不是原文，不能当 expect 基准"
        );
    }

    #[test]
    fn explicit_model_values_are_not_overwritten() {
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path":"/tmp/d.rs","content":"x\ny\n","tag":"1111"}).to_string(),
        );
        let mut input = json!({
            "path":"/tmp/d.rs","tag":"9999",
            "ops":[{"start_line":1,"expect":"model_said","new_content":"X"}]
        });
        heal_edit_input(&sc("t"), "edit", &mut input);
        assert_eq!(input["tag"], "9999", "模型显式给的 tag 不覆盖");
        assert_eq!(input["ops"][0]["expect"], "model_said", "模型显式给的 expect 不覆盖");
    }

    #[test]
    fn edit_refreshes_tag_only_and_keeps_content_belief() {
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path":"/tmp/e.rs","content":"p\nq\n","tag":"AAAA"}).to_string(),
        );
        // agent 自己编辑后，tag 变了但内容认知保留。
        record_tool_result(&sc("t"), "edit", &json!({"path":"/tmp/e.rs","tag":"BBBB"}).to_string());
        let mut input = json!({"path":"/tmp/e.rs","ops":[{"start_line":2,"new_content":"Z"}]});
        heal_edit_input(&sc("t"), "edit", &mut input);
        assert_eq!(input["tag"], "BBBB", "tag 应刷新为编辑后的新值（不误报 stale）");
        assert_eq!(input["ops"][0]["expect"], "q", "内容认知保留，供工具层重定位");
    }

    #[test]
    fn write_invalidates_ledger() {
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path":"/tmp/f.rs","content":"old\n","tag":"AAAA"}).to_string(),
        );
        record_tool_result(&sc("t"), "write", &json!({"path":"/tmp/f.rs"}).to_string());
        let mut input = json!({"path":"/tmp/f.rs","ops":[{"start_line":1,"new_content":"X"}]});
        let notes = heal_edit_input(&sc("t"), "edit", &mut input);
        assert!(input.get("tag").is_none(), "write 后台账作废，不应再补 tag");
        assert!(input["ops"][0].get("expect").is_none(), "write 后不应再补 expect");
        assert!(notes.is_empty());
    }

    #[test]
    fn unknown_file_and_non_edit_tools_are_untouched() {
        let mut input = json!({"path":"/tmp/never_read.rs","ops":[{"start_line":1,"new_content":"X"}]});
        assert!(heal_edit_input(&sc("t"), "edit", &mut input).is_empty(), "没读过 → 保持原样");
        assert!(input.get("tag").is_none());
        let mut other = json!({"path":"/tmp/a.rs"});
        assert!(heal_edit_input(&sc("t"), "search", &mut other).is_empty(), "非 edit 工具不处理");
    }

    #[test]
    fn insert_op_anchors_on_single_line() {
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path":"/tmp/g.rs","content":"one\ntwo\nthree\n","tag":"7777"}).to_string(),
        );
        let mut input = json!({
            "path":"/tmp/g.rs",
            "ops":[{"start_line":2,"end_line":99,"insert_after":true,"new_content":"X"}]
        });
        heal_edit_input(&sc("t"), "edit", &mut input);
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
        record_tool_result(&sc("t"), "read", &serde_json::to_string(&read_out).unwrap());

        // 2. 文件在前面被插入两行 → TARGET 漂到第 5 行（模型并不知道）。
        std::fs::write(&file, "new1\nnew2\na\nb\nTARGET\nd\n").unwrap();

        // 3. 模型按**旧**行号 3 发 edit，且完全没传 expect / tag。
        let mut input = json!({
            "path": path,
            "ops": [{"start_line": 3, "end_line": 3, "new_content": "HEALED"}]
        });
        let notes = heal_edit_input(&sc("t"), "edit", &mut input);
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
        record_tool_result(&sc("t"), "read", &serde_json::to_string(&read_out).unwrap());

        // OLD_LINE 被彻底删掉 → 无法重定位。
        let replaced = "x\nSOMETHING_ELSE\nz\n";
        std::fs::write(&file, replaced).unwrap();

        let mut input = json!({
            "path": path,
            "ops": [{"start_line": 2, "end_line": 2, "new_content": "NEW"}]
        });
        heal_edit_input(&sc("t"), "edit", &mut input);
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

    /// 锚点类失败按文件累计，第 3 次升级为「换策略」硬指令。
    ///
    /// 覆盖的正是 `AgentRunner` 现有熔断抓不到的形态：每次 `old_text` 都
    /// 不同（模型在猜锚点），连击/打转两条规则都不触发。
    #[test]
    fn anchor_failures_accumulate_per_file_and_escalate() {
        use latte_rs_agent_tools::tools::edit as et;
        let path = "/tmp/anchor-escalate-case.md";
        let input = json!({"path": path});
        let err = |code: &str| et::failure_code_test_error(code);

        // 前两次：软提示，只点「重读」。
        for i in 1..=2 {
            let note = note_edit_failure(&sc("t"), "edit", &input, &err(et::EDIT_ANCHOR_NOT_FOUND))
                .unwrap_or_else(|| panic!("第 {i} 次应有提示"));
            assert!(note.contains("[锚点提示]"), "第 {i} 次应是软提示: {note}");
            assert!(!note.contains("[停]"), "第 {i} 次不该升级: {note}");
        }
        // 第三次：升级，要求换策略。
        let note = note_edit_failure(&sc("t"), "edit", &input, &err(et::EDIT_AMBIGUOUS))
            .expect("第 3 次应有提示");
        assert!(note.contains("[停]"), "第 3 次应升级: {note}");
        assert!(note.contains("连续 3 次"), "应报出连击次数: {note}");
        assert!(note.contains("read"), "应给出重读这条出路: {note}");

        // 成功 read 之后清零 → 又回到软提示档。
        record_tool_result(
            &sc("t"),
            "read",
            &json!({"path": path, "content": "x\n", "tag": "AAAA"}).to_string(),
        );
        let note = note_edit_failure(&sc("t"), "edit", &input, &err(et::EDIT_ANCHOR_NOT_FOUND))
            .expect("清零后仍应有提示");
        assert!(
            note.contains("[锚点提示]") && !note.contains("[停]"),
            "成功 read 后必须清零连击: {note}"
        );
    }

    /// 只对锚点类失败计数：参数不合法是另一类毛病，混进来会让阈值失去含义。
    /// 非 edit 工具一律不管。
    #[test]
    fn non_anchor_failures_are_not_counted() {
        use latte_rs_agent_tools::tools::edit as et;
        let path = "/tmp/anchor-noncount-case.md";
        let input = json!({"path": path});
        assert!(
            note_edit_failure(
                &sc("t"),
                "edit",
                &input,
                &et::failure_code_test_error(et::EDIT_BAD_ARGS)
            )
            .is_none(),
            "参数类失败不该计入锚点连击"
        );
        assert!(
            note_edit_failure(
                &sc("t"),
                "read",
                &input,
                &et::failure_code_test_error(et::EDIT_ANCHOR_NOT_FOUND)
            )
            .is_none(),
            "非 edit 工具不该走这条记账"
        );
    }

    /// 作用域隔离：同一路径在不同 (session, role) 下互不可见。
    ///
    /// 这是符号门禁改用台账的前提。原来台账按裸路径、进程级全局，workflow
    /// 的 `survey` 步（programmer）读过的文件，`breakdown` 步（architect）
    /// 查台账也查得到——正是要拦的那类被放行。
    #[test]
    fn observations_are_scoped_per_session_and_role() {
        let path = "/tmp/scope-isolation-case.rs";
        let reader = Scope::new("sess-A", "programmer");
        let other_role = Scope::new("sess-A", "architect");
        let other_sess = Scope::new("sess-B", "programmer");

        record_tool_result(
            &reader,
            "read",
            &json!({"path": path, "content": "a\nb\nc\n", "tag": "AAAA"}).to_string(),
        );

        assert!(
            observed_lines(&reader, path).is_some(),
            "读过的角色应能查到"
        );
        assert!(
            observed_lines(&other_role, path).is_none(),
            "同会话里**别的角色**不该继承这份观察"
        );
        assert!(
            observed_lines(&other_sess, path).is_none(),
            "别的会话不该继承这份观察"
        );
    }

    /// `search` 的匹配行进 `seen_lines`，但**不**进 `regions`。
    ///
    /// 后半句是硬要求：`regions` 会被 `heal_edit_input` 当作 `expect` 的内容
    /// 来源，而 search 只回匹配行、拿不到连续原文；塞进去会补出假 expect、
    /// 把文件改错位置。
    #[test]
    fn search_records_provenance_without_polluting_content() {
        let scope = Scope::new("sess-search", "programmer");
        let path = "/tmp/search-prov-case.rs";
        record_tool_result(
            &scope,
            "search",
            &json!({
                "scopePath": ".",
                "matches": [
                    {"file": path, "line": 12, "content": "fn foo() {"},
                    {"file": path, "line": 40, "content": "fn bar() {"},
                    {"file": "/tmp/other-prov-case.rs", "line": 7, "content": "x"},
                ]
            })
            .to_string(),
        );

        let seen = observed_lines(&scope, path).expect("search 命中的文件应有来源记录");
        assert!(seen.contains(&12) && seen.contains(&40), "seen={seen:?}");
        assert!(!seen.contains(&13), "只记真的展示过的行: {seen:?}");
        assert!(
            observed_lines(&scope, "/tmp/other-prov-case.rs").is_some(),
            "同一次调用里的其它文件也要记"
        );

        // 关键：内容侧必须干净
        let snap = ledger()
            .read()
            .ok()
            .and_then(|lg| lg.map.get(&scope.key(path)).cloned())
            .expect("应有条目");
        assert!(snap.full.is_none(), "search 不该写 full");
        assert!(snap.regions.is_empty(), "search 绝不能写 regions（会补出假 expect）");
    }

    /// `code_graph` 的 `matches` 是渲染后的整段文本，按 `path:start-end:` 反解。
    #[test]
    fn code_graph_seen_lines_are_parsed_from_rendered_output() {
        let scope = Scope::new("sess-cg", "architect");
        let a = "include/jemalloc/internal/bit_util.h";
        record_tool_result(
            &scope,
            "code_graph",
            &json!({
                "lang": "c",
                "total_matches": 3,
                "shown": 3,
                "matches": format!(
                    "{a}:21-35: static inline unsigned ffs_llu(...)\n\
                     {a}:296: #error\n\
                     src/jemalloc.c:980-1063: static void obtain_malloc_conf(...)"
                ),
            })
            .to_string(),
        );

        let seen = observed_lines(&scope, a).expect("应有来源记录");
        assert!(seen.contains(&21) && seen.contains(&35), "跨度要全展开: {seen:?}");
        assert!(seen.contains(&296), "单行形态也要认");
        assert!(!seen.contains(&36), "跨度外不该算看过: {seen:?}");
        let seen_c = observed_lines(&scope, "src/jemalloc.c").expect("第二个文件也要记");
        assert!(seen_c.contains(&980) && seen_c.contains(&1063));
    }

    /// LRU（而不是 FIFO）：反复使用的条目不该被后来者挤掉。
    #[test]
    fn hot_entries_survive_eviction_under_lru() {
        let scope = Scope::new("sess-lru", "programmer");
        let hot = "/tmp/lru-hot.rs";
        let record = |path: &str| {
            record_tool_result(
                &scope,
                "read",
                &json!({"path": path, "content": "x\n", "tag": "AAAA"}).to_string(),
            )
        };
        record(hot);
        // 灌入超过容量上限的冷条目，期间不断"使用" hot。
        for i in 0..(MAX_ENTRIES + 20) {
            record(&format!("/tmp/lru-cold-{i}.rs"));
            if i % 4 == 0 {
                record(hot); // 触碰一次 → LRU 里挪到队尾
            }
        }
        assert!(
            observed_lines(&scope, hot).is_some(),
            "被反复使用的条目在 LRU 下必须存活（FIFO 下会被挤掉）"
        );
    }

    /// 超大内容不进台账，避免一次吃光字节预算。
    #[test]
    fn oversized_content_is_not_snapshotted() {
        let scope = Scope::new("sess-big", "programmer");
        let path = "/tmp/oversized-case.rs";
        let huge = "x".repeat(MAX_SNAPSHOT_FILE_BYTES + 1);
        record_tool_result(
            &scope,
            "read",
            &json!({"path": path, "content": huge, "tag": "AAAA"}).to_string(),
        );
        let snap = ledger()
            .read()
            .ok()
            .and_then(|lg| lg.map.get(&scope.key(path)).cloned());
        let full_len = snap.as_ref().and_then(|s| s.full.as_ref()).map(|f| f.len()).unwrap_or(0);
        assert!(
            full_len <= MAX_SNAPSHOT_FILE_BYTES,
            "超大文件的全文不该整份收进台账: {full_len}"
        );
    }
}
