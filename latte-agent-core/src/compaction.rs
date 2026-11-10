//! 自动上下文压缩（auto-compaction）。
//!
//! 骨架对齐 deepseek-harness 的 `compaction/*` 家族：**触发策略**、
//! **区间选择**、**摘要生成**、**历史替换** 四段分离，参数全部可配。
//!
//! # 为什么需要它
//!
//! 实测（jemalloc 会话）：单会话 22.19M 输入 token，而工具结果唯一内容
//! 只有 0.97M —— **22.7 倍放大**，单子会话最高 105 倍。两个正交的成本源：
//!
//! - **单价**：每轮重传全部历史，其中最大且最不变的前缀被反复计费 →
//!   由 prompt caching 解决（[`crate::agent`] 侧的 4 断点 + byte-stability）。
//! - **总量**：历史无限增长 → 由本模块解决（旧历史摘要成一条 checkpoint）。
//!
//! 已有的 `compact_stale_tool_results` 只缩短工具结果**文本**、不减少消息
//! 条数，覆盖约 30% 的内容（实测把 105x 降到 90x）。它相当于
//! deepseek-harness 的 `compaction-tool-result-pruner`——是本模块的**前置**
//! 减负手段，不是替代。
//!
//! # 与 caching 协同的关键设计
//!
//! 摘要调用本身也要花钱。deepseek-harness 的做法是把压缩指令作为**最后
//! 一条 user 消息**追加在原对话之后，而不是另起一个 summarizer system
//! prompt —— 这样这次辅助调用是上一次请求的**真前缀**，provider 的 KV
//! cache 被复用而不是失效。本模块沿用该做法（见
//! [`build_summary_request`]）。
//!
//! # 防摘要嵌套膨胀
//!
//! 摘要用 [`SUMMARY_OPEN_TAG`] 包裹，且指令明确要求：遇到已有的
//! checkpoint 要**合并**（保留仍成立的事实、丢弃过时的），不要原样复制
//! 往前堆。否则反复压缩会让摘要层层套娃、越压越大。

use latte_ai::models::{Message, Role};

/// 摘要块的包裹标签。用它而不是裸文本，是为了让下一次压缩能识别出
/// 「这段是上一次的 checkpoint」并做合并而非复制（见模块文档）。
pub const SUMMARY_OPEN_TAG: &str = "<compacted-summary>";
/// 摘要块的闭合标签。
pub const SUMMARY_CLOSE_TAG: &str = "</compacted-summary>";

// ─── 可配置参数 ──────────────────────────────────────────────────────

/// 触发压缩的上下文占用比例（默认 0.8 = 用到窗口 80%）。
///
/// 取 0.8 与 deepseek-harness 的 `DEFAULT_THRESHOLD_RATIO` 一致：留 20%
///
/// 同一比例也被 workflow 的动态 prompt 预算复用
/// （`workflow::resolve_prompt_budget` 在 step 只开
/// `context_overflow_restart` 而未显式配预算时的默认值）——两处都是
/// 「一个请求该占用窗口多少」这同一个问题的答案，不该各写一个魔数。
pub(crate) const DEFAULT_THRESHOLD_RATIO: f64 = 0.8;
/// 保留**逐字原文**的尾部轮数（默认 2）。
///
/// 对齐 opencode 的 `DEFAULT_TAIL_TURNS`。保留的是最近的完整往返，让模型
/// 手里始终有「刚刚在做什么」的原始细节，摘要只接管更早的部分。
const DEFAULT_KEEP_RECENT_TURNS: usize = 2;
/// 摘要输出的 token 上限（默认 2048）。摘要是结构化清单，不需要很长；
/// 给太大反而鼓励模型把原文抄进去、失去压缩意义。
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 2048;
/// 压缩后仍未降到阈值以下时，最多连续压几次（默认 1）。
///
/// 防「压不动还一直压」：每次压缩都是一次真实 LLM 调用，失控会烧钱。
const DEFAULT_MAX_PASSES: u8 = 1;

/// 环境变量读取：**只在显式设置且能解析时**返回 `Some`，供优先级链
/// （env > 配置段 > 默认）判断"这一层有没有给值"。
fn env_f64_opt(key: &str) -> Option<f64> {
    std::env::var(key).ok()?.trim().parse::<f64>().ok()
}

fn env_usize_opt(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.trim().parse::<usize>().ok()
}

fn env_u32_opt(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.trim().parse::<u32>().ok()
}

/// 压缩的触发来源。
///
/// 抄 deepseek-harness 的 `CompactionTrigger = 'pressure' | 'context-overflow'`
/// （`packages/compaction/compaction/src/index.ts:25`）。
///
/// 为什么必须有第二种：`Pressure` 的判据是 `est_tokens / context_window`，
/// 而 `context_window` 是**配置里声明的值**。声明写大了（实测 jemalloc 的
/// 三个模型全写 `context_window = 1000000`，阈值 0.8 → 800K）主动压缩就
/// 永不触发——那 7 个会话里 `ContextCompacted` 一次都没出现。被动触发不看
/// 声明值，只看供应商**真的拒了**，是不依赖配置正确性的兜底。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// 轮次入口的阈值检查（占用比例达到 `threshold_ratio`）。
    Pressure,
    /// 供应商已经报了「超出上下文窗口」，压缩后重试。
    ///
    /// 这条**跳过阈值判断**：既然请求已经被拒，占用比例算出来是多少都不
    /// 重要了。
    ContextOverflow,
}

/// 识别供应商的「超出上下文窗口」报错。
///
/// 移植 deepseek-harness 的 `isContextWindowExceededError`
/// （`packages/llm/llm/src/error.ts:80`）。各家 OpenAI 兼容端点措辞都不
/// 一样，靠一组模式覆盖，而不是指望统一的错误码。
///
/// 判据故意收紧：只认同时出现「超限动作」和「上下文/长度」两类词的组合。
/// 宁可漏判（退回原错误、行为与从前一致），也不要把普通 400 误判成溢出
/// ——误判会白烧一次摘要调用，还把好历史压掉。
pub fn is_context_overflow_error(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    // 「超出/过长」类动作词
    let exceeded = [
        "exceed",
        "exceeds",
        "exceeded",
        "overflow",
        "too long",
        "too large",
        "larger than",
        "maximum context",
        "max context",
        "context_length_exceeded",
        "超出",
        "超过",
        "过长",
    ]
    .iter()
    .any(|k| d.contains(k));
    // 「上下文/长度」类名词
    let context_noun = [
        "context length",
        "context window",
        "context_length",
        "context",
        "prompt",
        "input",
        "上下文",
        "长度",
    ]
    .iter()
    .any(|k| d.contains(k));
    // 明确的错误码单独放行（不需要凑措辞）
    if d.contains("context_length_exceeded") || d.contains("string_above_max_length") {
        return true;
    }
    exceeded && context_noun
}

/// 压缩策略：全部参数可配。
///
/// 读取顺序：环境变量 → 内置默认。所有数值都钳在安全区间内，配错不会
/// 把行为推到极端（如阈值 0 导致每轮都压、保留 0 轮导致模型失去上下文）。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPolicy {
    /// 总开关。`LATTE_AGENT_AUTO_COMPACT=0/false/no/off` 关闭。
    pub enabled: bool,
    /// 触发阈值（占 context_window 的比例），钳 `0.5..=0.95`。
    /// `LATTE_AGENT_COMPACT_THRESHOLD_RATIO`
    pub threshold_ratio: f64,
    /// 保留逐字原文的尾部轮数，钳 `1..=32`。
    /// `LATTE_AGENT_COMPACT_KEEP_RECENT_TURNS`
    pub keep_recent_turns: usize,
    /// 摘要输出 token 上限，钳 `256..=16384`。
    /// `LATTE_AGENT_COMPACT_SUMMARY_MAX_TOKENS`
    pub summary_max_tokens: u32,
    /// 单次检查内最多压几遍，钳 `1..=3`。
    /// `LATTE_AGENT_COMPACT_MAX_PASSES`
    pub max_passes: u8,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_ratio: DEFAULT_THRESHOLD_RATIO,
            keep_recent_turns: DEFAULT_KEEP_RECENT_TURNS,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            max_passes: DEFAULT_MAX_PASSES,
        }
    }
}

impl CompactionPolicy {
    /// 从环境变量解析策略（缺失项用默认值，越界值被钳住）。
    ///
    /// 等价于 `resolve(&Default::default())`——即没有配置文件时的行为。
    pub fn from_env() -> Self {
        Self::resolve(&crate::config::CompactionSection::default())
    }

    /// 按 **环境变量 > 配置段 > 内置默认** 的优先级解析策略。
    ///
    /// 配置段来自 `[compaction]`（project 层覆盖 global 层，合并在
    /// [`crate::config`] 完成）。环境变量留在最高优先级，是为了保留
    /// 「临时压测某个参数不用改文件」的能力——排查问题时最常用。
    ///
    /// 所有数值都钳在安全区间：配错（阈值 0 导致每轮都压、保留 0 轮导致
    /// 模型失去上下文）不会把行为推到极端。
    pub fn resolve(section: &crate::config::CompactionSection) -> Self {
        // 每一项：env 有值用 env；否则配置段有值用它（钳位）；否则默认。
        let enabled = match std::env::var("LATTE_AGENT_AUTO_COMPACT") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            Err(_) => section.enabled.unwrap_or(true),
        };
        Self {
            enabled,
            threshold_ratio: env_f64_opt("LATTE_AGENT_COMPACT_THRESHOLD_RATIO")
                .or(section.threshold_ratio)
                .unwrap_or(DEFAULT_THRESHOLD_RATIO)
                .clamp(0.5, 0.95),
            keep_recent_turns: env_usize_opt("LATTE_AGENT_COMPACT_KEEP_RECENT_TURNS")
                .or(section.keep_recent_turns)
                .unwrap_or(DEFAULT_KEEP_RECENT_TURNS)
                .clamp(1, 32),
            summary_max_tokens: env_u32_opt("LATTE_AGENT_COMPACT_SUMMARY_MAX_TOKENS")
                .or(section.summary_max_tokens)
                .unwrap_or(DEFAULT_SUMMARY_MAX_TOKENS)
                .clamp(256, 16_384),
            max_passes: env_u32_opt("LATTE_AGENT_COMPACT_MAX_PASSES")
                .map(|v| v as u8)
                .or(section.max_passes)
                .unwrap_or(DEFAULT_MAX_PASSES)
                .clamp(1, 3),
        }
    }

    /// 给定上下文窗口，返回触发压缩的 token 阈值。
    pub fn threshold_tokens(&self, context_window: u32) -> u32 {
        ((context_window as f64) * self.threshold_ratio) as u32
    }

    /// 当前估算用量是否已达压缩阈值。
    ///
    /// `context_window == 0`（目录没配窗口）时恒不触发——没有分母就没有
    /// 「压力」可言，宁可不压也不要按猜的窗口误压。
    pub fn should_compact(&self, est_tokens: u32, context_window: u32) -> bool {
        self.enabled && context_window > 0 && est_tokens >= self.threshold_tokens(context_window)
    }
}

// ─── 区间选择 ────────────────────────────────────────────────────────

/// 选出可被摘要替换的历史区间 `[0, end)`。
///
/// 规则：
/// 1. 保留最近 `keep_recent_turns` 个「轮」的原文。一轮 = 一条 user 消息
///    起、到下一条 user 消息前为止（含其间的 assistant / tool 消息）。
/// 2. **保住 tool_call ↔ tool_result 配对**：切点不能落在一对中间，否则
///    provider 直接拒（孤儿 tool_result）。做法是把切点向前退到最近的
///    「非 Tool 消息」边界。
/// 3. 前导的 System 消息永不进入压缩区间——它是缓存前缀的一部分，且是
///    角色定义，摘要掉等于把 agent 的身份弄丢。
///
/// 返回 `None` 表示没有可压的区间（历史太短 / 全是要保留的尾部）。
pub fn select_compaction_end(messages: &[Message], keep_recent_turns: usize) -> Option<usize> {
    // 跳过前导 System（不参与压缩）。
    let head = messages
        .iter()
        .position(|m| m.role != Role::System)
        .unwrap_or(messages.len());
    if head >= messages.len() {
        return None;
    }

    // 从尾部往前数 user 消息：第 `keep_recent_turns` 个（从后数）就是要
    // 保留的最早一轮的起点，切点落在它之前。数到就停——继续往前会把切点
    // 推到更早，等于多保留了一轮。
    let mut user_seen = 0usize;
    let mut cut = None;
    for idx in (head..messages.len()).rev() {
        if messages[idx].role == Role::User {
            user_seen += 1;
            if user_seen == keep_recent_turns {
                cut = Some(idx);
                break;
            }
        }
    }
    // 数不满 keep_recent_turns 轮 → 整段都要保留，无可压区间。
    let mut end = cut?;

    // 配对保护：切点不能落在 tool_result 上（它的 tool_call 在更早处）。
    // 向前退到第一个非 Tool 消息之后。
    while end > head && messages[end.saturating_sub(1)].role == Role::Tool {
        end -= 1;
    }
    // 再退过产生这些 tool_result 的 assistant（它带 tool_calls）。
    while end > head
        && messages
            .get(end.saturating_sub(1))
            .map(|m| m.tool_calls.is_some())
            .unwrap_or(false)
    {
        end -= 1;
    }

    if end <= head {
        return None;
    }
    Some(end)
}

// ─── 摘要模板 ────────────────────────────────────────────────────────

/// 摘要模板的段落定义：`(标题, 该段要写什么)`。
///
/// 8 段取自 deepseek-harness 的 `COMPACTION_INSTRUCTION`（比 opencode 的
/// 7 段多了 "Errors and Fixes"，对编码 agent 很关键——踩过的坑不该在压缩
/// 后重新踩一遍）。可通过 [`summary_template_sections`] 定制。
pub const DEFAULT_TEMPLATE_SECTIONS: &[(&str, &str)] = &[
    (
        "Primary Request and Intent",
        "用户最初与演进中的目标；措辞关键处原样引用",
    ),
    ("Key Technical Concepts", "涉及的技术、框架、模式与约定"),
    ("Files and Code", "精确路径：为什么重要、关键改动或片段"),
    ("Errors and Fixes", "报错：怎么解决的，以及相关的用户反馈"),
    ("Pending Jobs", "明确要求但尚未完成的工作"),
    ("Current Work", "此 checkpoint 时点正在进行的事"),
    ("Next Step", "紧接最近一次请求的下一个动作，或「(none)」"),
    (
        "Critical Context",
        "决策与其理由、约束、用户偏好、待解问题、继续所需的数据",
    ),
];

/// 解析摘要模板段落，优先级 **环境变量 > 配置段 > 内置 8 段**。
///
/// - 环境变量：`LATTE_AGENT_COMPACT_SECTIONS="Goal|Progress|Next"`（`|` 分隔）
/// - 配置段：`[compaction] sections = ["Goal", "Progress", "Next"]`
///
/// 自定义时只给标题、不给说明——段落语义由标题自解释。
pub fn resolve_template_sections(
    section: &crate::config::CompactionSection,
) -> Vec<(String, String)> {
    if let Ok(raw) = std::env::var("LATTE_AGENT_COMPACT_SECTIONS") {
        let titles: Vec<(String, String)> = raw
            .split('|')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|t| (t.to_string(), String::new()))
            .collect();
        if !titles.is_empty() {
            return titles;
        }
    }
    if let Some(list) = section.sections.as_ref() {
        let titles: Vec<(String, String)> = list
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|t| (t.to_string(), String::new()))
            .collect();
        if !titles.is_empty() {
            return titles;
        }
    }
    DEFAULT_TEMPLATE_SECTIONS
        .iter()
        .map(|(t, d)| (t.to_string(), d.to_string()))
        .collect()
}

/// 只按环境变量 / 默认解析（无配置段时的便捷入口）。
pub fn summary_template_sections() -> Vec<(String, String)> {
    resolve_template_sections(&crate::config::CompactionSection::default())
}

/// 渲染压缩指令（作为**最后一条 user 消息**发出，见模块文档的缓存说明）。
pub fn compaction_instruction(sections: &[(String, String)]) -> String {
    let mut s = String::new();
    s.push_str(
        "你现在是这个编码助手的压缩引擎。把**上面**的对话压缩成一份结构化 checkpoint，\
         让另一个模型能据此无损地接着干活。\n\n\
         严格按下面的 Markdown 结构输出：每一节都要保留、顺序不变。用简洁的要点，不要大段散文。\
         某节为空就写「(none)」——**永远不要删掉某一节**。\n\n",
    );
    for (title, desc) in sections {
        s.push_str(&format!("## {title}\n"));
        if desc.is_empty() {
            s.push_str("- [...]\n\n");
        } else {
            s.push_str(&format!("- [{desc}]\n\n"));
        }
    }
    s.push_str(&format!(
        "规则：\n\
         - 保留精确的文件路径、命令、报错串、标识符、数值、函数签名与语法片段。\n\
         - 忠实记录用户反馈与明确指示，尤其是纠正。\n\
         - 不要提到这次压缩请求、也不要说上下文被压缩过。\n\
         - 只输出 checkpoint 正文：不要调用任何工具、不要做别的动作。\n\
         - 如果对话里已经有 {SUMMARY_OPEN_TAG} 块，那是**上一次的 checkpoint**：\
           不要原样复制它，而要保留仍然成立的事实、丢弃过时的、把新信息合并进去，\
           最终仍是同一结构下的**单一**摘要。\n"
    ));
    s
}

/// 替换进历史的 checkpoint 消息文案：把摘要包进标签，并说明它是既有背景。
pub fn checkpoint_message_text(summary: &str) -> String {
    format!(
        "{SUMMARY_OPEN_TAG}\n\
         这是自动生成的 checkpoint，压缩了更早的一段对话以腾出上下文。\
         把其中内容当作**已确立的背景**直接用，不要复述、也不要向用户提起这个 checkpoint，\
         直接从后面的消息继续任务。\n\n\
         {summary}\n\
         {SUMMARY_CLOSE_TAG}"
    )
}

/// 构造摘要请求的消息序列：**原对话前缀 + 压缩指令**。
///
/// 关键（对齐 deepseek-harness）：指令追加在**最后**，前面原样保留 system
/// 与被压区间的消息。这样这次辅助调用是上一次请求的真前缀，provider 的
/// KV cache 命中而不是失效——摘要开销也压到缓存价。
pub fn build_summary_request(
    messages: &[Message],
    end: usize,
    sections: &[(String, String)],
) -> Vec<Message> {
    let mut out: Vec<Message> = messages[..end.min(messages.len())].to_vec();
    out.push(Message::user(compaction_instruction(sections)));
    out
}

#[cfg(test)]
mod tests {
    /// 读 env 的用例共用的锁。
    ///
    /// `CompactionPolicy::resolve` 会读 env，而单测在同进程内并行跑：
    /// `env_overrides_the_config_section` 设了
    /// `LATTE_AGENT_COMPACT_THRESHOLD_RATIO=0.9`，会让并行的钳位用例读到
    /// 0.9 而不是配置里的值，随机挂（既有缺陷，与本次改动无关）。凡是
    /// 依赖 env 处于已知状态的用例都要持这把锁。

    use super::*;

    fn user(t: &str) -> Message {
        Message::user(t)
    }
    fn asst(t: &str) -> Message {
        Message::assistant(t)
    }
    fn sys(t: &str) -> Message {
        Message::system(t)
    }

    // ─── 优先级：env > 配置段 > 默认 ─────────────────────────────────

    #[test]
    fn config_section_drives_policy_when_no_env() {
        let section = crate::config::CompactionSection {
            enabled: Some(true),
            threshold_ratio: Some(0.6),
            keep_recent_turns: Some(5),
            summary_max_tokens: Some(4096),
            max_passes: Some(2),
            sections: Some(vec!["Goal".into(), "Next".into()]),
        };
        let p = CompactionPolicy::resolve(&section);
        assert_eq!(p.threshold_ratio, 0.6);
        assert_eq!(p.keep_recent_turns, 5);
        assert_eq!(p.summary_max_tokens, 4096);
        assert_eq!(p.max_passes, 2);

        let secs = resolve_template_sections(&section);
        assert_eq!(secs.len(), 2, "配置段的自定义模板应生效: {secs:?}");
        assert_eq!(secs[0].0, "Goal");
    }

    #[test]
    fn unset_section_falls_back_to_builtin_defaults() {
        let _guard = crate::test_util::env_lock();
        unsafe { std::env::remove_var("LATTE_AGENT_COMPACT_THRESHOLD_RATIO") };
        let p = CompactionPolicy::resolve(&Default::default());
        assert_eq!(p, CompactionPolicy::default(), "未配置应等于内置默认");
        assert_eq!(
            resolve_template_sections(&Default::default()).len(),
            DEFAULT_TEMPLATE_SECTIONS.len(),
            "未配置应用内置 8 段"
        );
    }

    #[test]
    fn out_of_range_config_is_clamped_not_honoured() {
        let _guard = crate::test_util::env_lock();
        unsafe { std::env::remove_var("LATTE_AGENT_COMPACT_THRESHOLD_RATIO") };
        // 配错不该把行为推到极端（阈值 0 每轮都压 / 保留 0 轮丢上下文）。
        let section = crate::config::CompactionSection {
            threshold_ratio: Some(0.01),
            keep_recent_turns: Some(0),
            summary_max_tokens: Some(10),
            max_passes: Some(99),
            ..Default::default()
        };
        let p = CompactionPolicy::resolve(&section);
        assert_eq!(p.threshold_ratio, 0.5, "钳到下限");
        assert_eq!(p.keep_recent_turns, 1, "至少保留 1 轮");
        assert_eq!(p.summary_max_tokens, 256, "钳到下限");
        assert_eq!(p.max_passes, 3, "钳到上限");
    }

    #[test]
    fn env_overrides_the_config_section() {
        let _guard = crate::test_util::env_lock();
        // env 留在最高优先级：排查问题时不用改文件就能压测某个参数。
        let section = crate::config::CompactionSection {
            threshold_ratio: Some(0.6),
            keep_recent_turns: Some(5),
            ..Default::default()
        };
        std::env::set_var("LATTE_AGENT_COMPACT_THRESHOLD_RATIO", "0.9");
        let p = CompactionPolicy::resolve(&section);
        std::env::remove_var("LATTE_AGENT_COMPACT_THRESHOLD_RATIO");
        assert_eq!(p.threshold_ratio, 0.9, "env 必须覆盖配置段");
        assert_eq!(p.keep_recent_turns, 5, "未被 env 覆盖的项仍取配置段");
    }

    #[test]
    fn section_is_unset_detects_a_blank_block() {
        // 分层合并靠它判断「这一层整段没配、该取下层值」。
        assert!(crate::config::CompactionSection::default().is_unset());
        assert!(!crate::config::CompactionSection {
            enabled: Some(false),
            ..Default::default()
        }
        .is_unset());
    }

    // ─── 阈值 ────────────────────────────────────────────────────────

    #[test]
    fn threshold_is_a_ratio_of_the_window() {
        let p = CompactionPolicy::default();
        assert_eq!(p.threshold_tokens(100_000), 80_000, "默认 0.8");
        assert!(!p.should_compact(79_999, 100_000));
        assert!(p.should_compact(80_000, 100_000), "达到阈值即触发");
    }

    #[test]
    fn no_window_means_never_compact() {
        // 目录没配 context_window 时没有分母，宁可不压也不按猜的窗口误压。
        let p = CompactionPolicy::default();
        assert!(!p.should_compact(u32::MAX, 0));
    }

    #[test]
    fn disabled_policy_never_compacts() {
        let p = CompactionPolicy {
            enabled: false,
            ..Default::default()
        };
        assert!(!p.should_compact(999_999, 1000));
    }

    // ─── 区间选择 ────────────────────────────────────────────────────

    #[test]
    fn keeps_the_recent_turns_verbatim() {
        // 4 轮对话，保留最近 2 轮 → 压掉前 2 轮。
        let msgs = vec![
            sys("role"),
            user("u1"),
            asst("a1"),
            user("u2"),
            asst("a2"),
            user("u3"),
            asst("a3"),
            user("u4"),
            asst("a4"),
        ];
        let end = select_compaction_end(&msgs, 2).expect("应有可压区间");
        // 保留 u3/a3/u4/a4 → 切点在 u3 之前（下标 5）。
        assert_eq!(end, 5, "切点应恰好在倒数第 2 条 user 之前");
        assert_eq!(msgs[end].as_text(), "u3");
    }

    #[test]
    fn system_head_is_never_compacted() {
        let msgs = vec![sys("role"), user("u1"), asst("a1"), user("u2"), asst("a2")];
        let end = select_compaction_end(&msgs, 1).expect("有区间");
        assert!(end >= 1, "System 不得进入压缩区间，切点必须在它之后: {end}");
    }

    #[test]
    fn short_history_has_nothing_to_compact() {
        let msgs = vec![sys("role"), user("u1"), asst("a1")];
        assert!(
            select_compaction_end(&msgs, 2).is_none(),
            "只有 1 轮、要保留 2 轮 → 无可压区间"
        );
    }

    #[test]
    fn cut_point_never_splits_a_tool_pair() {
        // assistant 带 tool_calls，随后是 tool 结果。切点若落在 tool 上，
        // provider 会因孤儿 tool_result 直接拒。
        let mut a = asst("calling");
        a.tool_calls = Some(vec![latte_ai::models::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
            arguments_raw: None,
            arguments_parse_error: None,
        }]);
        let mut t = Message::user("result");
        t.role = Role::Tool;
        t.tool_call_id = Some("c1".into());

        let msgs = vec![
            sys("role"),
            user("u1"),
            a,
            t,
            user("u2"),
            asst("a2"),
            user("u3"),
            asst("a3"),
        ];
        let end = select_compaction_end(&msgs, 2).expect("有区间");
        // 切点前一条不能是 Tool，也不能是带 tool_calls 的 assistant。
        assert_ne!(
            msgs[end - 1].role,
            Role::Tool,
            "切点不能紧跟在 tool_result 后断开配对"
        );
        assert!(
            msgs[end - 1].tool_calls.is_none(),
            "切点不能把带 tool_calls 的 assistant 与它的结果分开"
        );
    }

    // ─── 模板与指令 ──────────────────────────────────────────────────

    #[test]
    fn default_template_has_eight_sections() {
        assert_eq!(DEFAULT_TEMPLATE_SECTIONS.len(), 8);
        let titles: Vec<&str> = DEFAULT_TEMPLATE_SECTIONS.iter().map(|(t, _)| *t).collect();
        assert!(
            titles.contains(&"Errors and Fixes"),
            "编码 agent 必须留这一节: {titles:?}"
        );
        assert!(titles.contains(&"Next Step"));
    }

    #[test]
    fn instruction_renders_every_section_and_the_merge_rule() {
        let sections = summary_template_sections();
        let text = compaction_instruction(&sections);
        for (title, _) in &sections {
            assert!(text.contains(&format!("## {title}")), "缺段落 {title}");
        }
        assert!(
            text.contains(SUMMARY_OPEN_TAG),
            "必须给出合并旧 checkpoint 的规则"
        );
        assert!(text.contains("(none)"), "空节要写 (none) 而不是删掉");
    }

    #[test]
    fn checkpoint_text_is_tagged_for_future_merges() {
        let t = checkpoint_message_text("## Goal\n- x");
        assert!(t.starts_with(SUMMARY_OPEN_TAG), "必须带开标签供下次识别");
        assert!(t.trim_end().ends_with(SUMMARY_CLOSE_TAG));
        assert!(t.contains("已确立的背景"));
    }

    #[test]
    fn summary_request_is_a_prefix_plus_instruction() {
        // 缓存复用的关键：前缀原样、指令在最后一条。
        let msgs = vec![sys("role"), user("u1"), asst("a1"), user("u2")];
        let req = build_summary_request(&msgs, 3, &summary_template_sections());
        assert_eq!(req.len(), 4, "3 条前缀 + 1 条指令");
        assert_eq!(req[0].as_text(), "role", "前缀逐条原样保留");
        assert_eq!(req[1].as_text(), "u1");
        assert_eq!(req[2].as_text(), "a1");
        assert_eq!(req[3].role, Role::User, "指令必须是最后一条 user 消息");
        assert!(req[3].as_text().contains("压缩引擎"));
    }

    /// 溢出分类器要覆盖各家 OpenAI 兼容端点的措辞，也要**不误判**普通错误。
    ///
    /// 移植 deepseek-harness 的 `isContextWindowExceededError`。误判的代价
    /// 是白烧一次摘要调用并压掉好历史，所以宁可漏判。
    #[test]
    fn context_overflow_classifier_covers_provider_wordings() {
        for msg in [
            "API error: 400 - context window exceeds limit",
            "This model's maximum context length is 128000 tokens",
            "error code: context_length_exceeded",
            "Input is too long for this model",
            "prompt too large for context window",
            "请求的上下文长度超出模型上限",
            "输入过长，超过上下文窗口",
        ] {
            assert!(is_context_overflow_error(msg), "应判为溢出: {msg}");
        }
        for msg in [
            "API error: 502 - Bad Gateway",
            "rate limit exceeded, please retry later",
            "insufficient quota",
            "invalid api key",
            "stream idle timeout (120s)",
            "tool_calls must be followed by tool messages",
        ] {
            assert!(!is_context_overflow_error(msg), "不该判为溢出: {msg}");
        }
    }

    /// 被动触发必须**跳过**阈值判断——那正是它存在的理由。
    ///
    /// 实测 jemalloc：三个模型都声明 `context_window = 1000000`，阈值 0.8
    /// → 800K，而单次请求远达不到，7 个会话 `ContextCompacted` 零次。声明
    /// 值不可信时，只有「供应商真的拒了」这个信号是可靠的。
    #[test]
    fn pressure_respects_threshold_but_overflow_does_not() {
        let _guard = crate::test_util::env_lock();
        unsafe { std::env::remove_var("LATTE_AGENT_COMPACT_THRESHOLD_RATIO") };
        let policy = CompactionPolicy::resolve(&crate::config::CompactionSection::default());
        // 1M 窗口下 10K 占用远低于 0.8 阈值
        assert!(
            !policy.should_compact(10_000, 1_000_000),
            "主动触发在阈值下不该压"
        );
        // 触发枚举本身要能区分两者（被动分支在 agent 侧跳过 should_compact）
        assert_ne!(
            CompactionTrigger::Pressure,
            CompactionTrigger::ContextOverflow
        );
        // 窗口未声明（0）时主动触发恒不压——没有分母就没有压力
        assert!(
            !policy.should_compact(999_999, 0),
            "没有 context_window 时主动触发不该压"
        );
    }
}
