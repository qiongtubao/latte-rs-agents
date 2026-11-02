//! `latte-agent debug usage`：把一个 UI 会话的模型调用按 **步骤 / 角色 / 模型** 汇总成
//! token 与耗时报告，用来排查「钱和时间花在哪、哪里在白烧」。
//!
//! 数据源（都在项目的 `.latte/ui-sessions/` 下，纯只读）：
//!
//! - `<sid>.jsonl`：会话主事件流。其中的 `RoleStarted { detail: "workflow step 'X'", sub_id }`
//!   把子会话和 workflow 步骤对上；
//! - `<sid>/<sub_id>.jsonl`：每个子会话（subagent / advisor）的 trace，一行一个 `TraceEvent`。
//!   `ModelCall` 带**逐次**用量（`usage`），`TurnEnd` 带整个 turn 的合计。
//!
//! 旧日志的 `ModelCall` 没有 `usage`：这时回退到该子会话的 `TurnEnd` 合计，报告里标「仅 turn 合计」，
//! 只能看总量、看不出每轮重传。
//!
//! 口径：`cache_read` 是 `input` 的**子集**（命中缓存的部分），新付费输入 = `input - cache_read`。
//! 放大倍数 = `input / output`——工具循环每一轮都把全部历史重传一遍，这个比值是「重传放大」的直接度量。

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::Value;

/// 一类模型调用的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallClass {
    /// 正常结束（stop / tool_calls / end_turn …）。
    Ok,
    /// 输出被长度上限截断（`length`）——产出是半截的，通常意味着要续写或重来。
    Truncated,
    Timeout,
    RateLimited,
    /// 余额不足 / 配额耗尽（402）。
    Billing,
    Error,
}

impl CallClass {
    pub fn of(finish_reason: &str) -> Self {
        let f = finish_reason.to_ascii_lowercase();
        if f == "length" || f.contains("max_tokens") {
            Self::Truncated
        } else if f.contains("timeout") {
            Self::Timeout
        } else if f.contains("429") || f.contains("rate_limit") {
            Self::RateLimited
        } else if f.contains("402") || f.contains("余额") || f.contains("insufficient") {
            Self::Billing
        } else if f.starts_with("error") {
            Self::Error
        } else {
            Self::Ok
        }
    }

    fn is_failure(self) -> bool {
        !matches!(self, Self::Ok | Self::Truncated)
    }
}

/// 一组调用的累计数据。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Agg {
    pub calls: u64,
    pub ok: u64,
    pub truncated: u64,
    pub timeout: u64,
    pub rate_limited: u64,
    pub billing: u64,
    pub error: u64,
    /// 成功调用的累计时延 / 失败调用（超时、限流、报错…）白白等掉的时延，毫秒。
    pub ok_ms: u64,
    pub fail_ms: u64,
    pub input: u64,
    pub output: u64,
    pub thinking: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 单次调用输入 token 的最大值（看历史涨到多大）。
    pub max_input: u64,
    /// 子会话数。
    pub subs: u64,
    /// 是否有部分数据只来自 `TurnEnd` 合计（没有逐次用量）。
    pub turn_totals_only: bool,
}

impl Agg {
    fn add_call(&mut self, class: CallClass, latency_ms: u64, u: &Usage) {
        self.calls += 1;
        match class {
            CallClass::Ok => self.ok += 1,
            CallClass::Truncated => self.truncated += 1,
            CallClass::Timeout => self.timeout += 1,
            CallClass::RateLimited => self.rate_limited += 1,
            CallClass::Billing => self.billing += 1,
            CallClass::Error => self.error += 1,
        }
        if class.is_failure() {
            self.fail_ms += latency_ms;
        } else {
            self.ok_ms += latency_ms;
        }
        self.add_usage(u);
        self.max_input = self.max_input.max(u.input);
    }

    fn add_usage(&mut self, u: &Usage) {
        self.input += u.input;
        self.output += u.output;
        self.thinking += u.thinking;
        self.cache_read += u.cache_read;
        self.cache_write += u.cache_write;
    }

    fn merge(&mut self, o: &Agg) {
        self.calls += o.calls;
        self.ok += o.ok;
        self.truncated += o.truncated;
        self.timeout += o.timeout;
        self.rate_limited += o.rate_limited;
        self.billing += o.billing;
        self.error += o.error;
        self.ok_ms += o.ok_ms;
        self.fail_ms += o.fail_ms;
        self.input += o.input;
        self.output += o.output;
        self.thinking += o.thinking;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.max_input = self.max_input.max(o.max_input);
        self.subs += o.subs;
        self.turn_totals_only |= o.turn_totals_only;
    }

    /// 新付费输入（扣掉缓存命中）。
    pub fn fresh_input(&self) -> u64 {
        self.input.saturating_sub(self.cache_read)
    }

    /// 缓存命中率（占输入）。没有输入返回 None。
    pub fn cache_hit(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cache_read as f64 / self.input as f64)
    }

    /// 输入 / 输出放大倍数。没有输出返回 None。
    pub fn amplification(&self) -> Option<f64> {
        (self.output > 0).then(|| self.input as f64 / self.output as f64)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    input: u64,
    output: u64,
    thinking: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Usage {
    fn from_call(v: &Value) -> Self {
        let g = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input: g("input_tokens"),
            output: g("output_tokens"),
            thinking: g("thinking_tokens"),
            cache_read: g("cache_read_tokens"),
            cache_write: g("cache_write_tokens"),
        }
    }

    fn from_turn_end(v: &Value) -> Self {
        let g = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input: g("total_input"),
            output: g("total_output"),
            thinking: g("total_thinking"),
            cache_read: g("total_cache_read"),
            cache_write: g("total_cache_write"),
        }
    }

    fn is_zero(&self) -> bool {
        self.input == 0 && self.output == 0 && self.thinking == 0
    }
}

/// 一个子会话的汇总。
#[derive(Debug, Clone, Default)]
pub struct SubSummary {
    pub sub_id: String,
    pub role: String,
    pub step: String,
    pub agg: Agg,
    /// 第一次 / 最后一次调用的输入 token（逐次用量才有）：看历史重传涨了多少。
    pub first_input: u64,
    pub last_input: u64,
}

#[derive(Debug, Clone, Default)]
pub struct UsageReport {
    pub total: Agg,
    pub by_step: BTreeMap<String, Agg>,
    pub by_role: BTreeMap<String, Agg>,
    pub by_model: BTreeMap<String, Agg>,
    pub subs: Vec<SubSummary>,
}

/// 从 `RoleStarted.detail`（如 `workflow step 'ch_logic'`）里取步骤 id。
fn step_of_detail(detail: &str) -> Option<String> {
    let rest = detail.split("step '").nth(1)?;
    Some(rest.split('\'').next()?.to_string())
}

/// 主事件流里 `sub_id → 步骤` 的映射。
pub fn step_map(main: &[Value]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for e in main {
        if e.get("type").and_then(Value::as_str) != Some("RoleStarted") {
            continue;
        }
        let (Some(sub), Some(detail)) = (
            e.get("sub_id").and_then(Value::as_str),
            e.get("detail").and_then(Value::as_str),
        ) else {
            continue;
        };
        if let Some(step) = step_of_detail(detail) {
            m.insert(sub.to_string(), step);
        }
    }
    m
}

/// 汇总。`subs` 是 `(sub_id, 该子会话 trace 的每一行)`。
pub fn analyze(main: &[Value], subs: Vec<(String, Vec<Value>)>) -> UsageReport {
    let steps = step_map(main);
    let mut report = UsageReport::default();
    for (sub_id, events) in subs {
        let mut role = String::new();
        let mut calls: Vec<(CallClass, u64, Usage, String)> = Vec::new();
        let mut turn_totals = Usage::default();
        for e in &events {
            if let Some(r) = e.get("role_name").and_then(Value::as_str) {
                role = r.to_string();
            }
            if let Some(mc) = e.get("ModelCall") {
                let class = CallClass::of(mc.get("finish_reason").and_then(Value::as_str).unwrap_or(""));
                let ms = mc.get("latency_ms").and_then(Value::as_u64).unwrap_or(0);
                let model = mc.get("model_id").and_then(Value::as_str).unwrap_or("?").to_string();
                let usage = mc.get("usage").map(Usage::from_call).unwrap_or_default();
                calls.push((class, ms, usage, model));
            }
            if let Some(te) = e.get("TurnEnd") {
                let t = Usage::from_turn_end(te);
                turn_totals.input += t.input;
                turn_totals.output += t.output;
                turn_totals.thinking += t.thinking;
                turn_totals.cache_read += t.cache_read;
                turn_totals.cache_write += t.cache_write;
            }
        }
        if calls.is_empty() && turn_totals.is_zero() {
            continue;
        }
        let per_call_usage = calls.iter().any(|(_, _, u, _)| !u.is_zero());
        let step = steps
            .get(&sub_id)
            .cloned()
            .unwrap_or_else(|| if role == "advisor" { "(advisor 审查)".into() } else { "(未归属)".into() });
        let mut agg = Agg { subs: 1, ..Default::default() };
        let (mut first_input, mut last_input) = (0, 0);
        for (class, ms, usage, model) in &calls {
            agg.add_call(*class, *ms, usage);
            let mut one = Agg::default();
            one.add_call(*class, *ms, usage);
            report.by_model.entry(model.clone()).or_default().merge(&one);
            if usage.input > 0 {
                if first_input == 0 {
                    first_input = usage.input;
                }
                last_input = usage.input;
            }
        }
        if !per_call_usage && !turn_totals.is_zero() {
            // 旧日志：没有逐次用量，只能用 turn 合计顶上。
            agg.add_usage(&turn_totals);
            agg.max_input = agg.max_input.max(turn_totals.input);
            agg.turn_totals_only = true;
        }
        report.total.merge(&agg);
        report.by_step.entry(step.clone()).or_default().merge(&agg);
        report.by_role.entry(if role.is_empty() { "?".into() } else { role.clone() }).or_default().merge(&agg);
        report.subs.push(SubSummary { sub_id, role, step, agg, first_input, last_input });
    }
    report
}

fn n(v: u64) -> String {
    // 千分位，便于一眼看量级。
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn pct(v: Option<f64>) -> String {
    v.map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "-".into())
}

fn ratio(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.0}x")).unwrap_or_else(|| "-".into())
}

fn secs(ms: u64) -> String {
    format!("{}s", ms / 1000)
}

fn table(out: &mut String, title: &str, rows: &BTreeMap<String, Agg>) {
    let _ = writeln!(out, "\n── {title} ──");
    let _ = writeln!(
        out,
        "{:<22} {:>5} {:>13} {:>11} {:>6} {:>7} {:>8} {:>8}",
        "名称", "调用", "输入token", "输出token", "缓存", "放大", "耗时", "白等"
    );
    let mut v: Vec<(&String, &Agg)> = rows.iter().collect();
    v.sort_by(|a, b| b.1.input.cmp(&a.1.input));
    for (name, a) in v {
        let mark = if a.turn_totals_only { "*" } else { "" };
        let _ = writeln!(
            out,
            "{:<22} {:>5} {:>13} {:>11} {:>6} {:>7} {:>8} {:>8}",
            format!("{name}{mark}"),
            a.calls,
            n(a.input),
            n(a.output),
            pct(a.cache_hit()),
            ratio(a.amplification()),
            secs(a.ok_ms + a.fail_ms),
            secs(a.fail_ms),
        );
    }
}

/// 渲染成纯文本报告。
pub fn render(r: &UsageReport) -> String {
    let t = &r.total;
    let mut out = String::new();
    let _ = writeln!(out, "=== token / 耗时报告 ===");
    let _ = writeln!(
        out,
        "子会话 {}  模型调用 {}（成功 {}，截断 {}，超时 {}，限流 {}，欠费 {}，其它错误 {}）",
        t.subs, t.calls, t.ok, t.truncated, t.timeout, t.rate_limited, t.billing, t.error
    );
    let _ = writeln!(
        out,
        "输入 {}（缓存命中 {} = {}，新付费 {}）  输出 {}  思考 {}  放大 {}",
        n(t.input),
        n(t.cache_read),
        pct(t.cache_hit()),
        n(t.fresh_input()),
        n(t.output),
        n(t.thinking),
        ratio(t.amplification()),
    );
    let _ = writeln!(
        out,
        "模型累计耗时 {}（其中失败调用白等 {}，占 {}）  单次最大输入 {}",
        secs(t.ok_ms + t.fail_ms),
        secs(t.fail_ms),
        pct((t.ok_ms + t.fail_ms > 0).then(|| t.fail_ms as f64 / (t.ok_ms + t.fail_ms) as f64)),
        n(t.max_input),
    );
    table(&mut out, "按步骤（输入 token 从多到少）", &r.by_step);
    table(&mut out, "按角色", &r.by_role);
    table(&mut out, "按模型", &r.by_model);

    // 最烧的几个子会话：输入 token 最多，附「历史涨了多少」。
    let mut subs: Vec<&SubSummary> = r.subs.iter().collect();
    subs.sort_by(|a, b| b.agg.input.cmp(&a.agg.input));
    let _ = writeln!(out, "\n── 输入 token 最多的子会话 ──");
    for s in subs.iter().take(6) {
        let growth = if s.first_input > 0 && s.last_input > s.first_input {
            format!("，单次输入 {} → {}", n(s.first_input), n(s.last_input))
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{:<28} {:<10} {:<16} 调用 {:>3}  输入 {:>11}  输出 {:>8}  放大 {}{}",
            s.sub_id,
            s.role,
            s.step,
            s.agg.calls,
            n(s.agg.input),
            n(s.agg.output),
            ratio(s.agg.amplification()),
            growth
        );
    }

    // 浪费信号：只列真有的。
    let mut flags: Vec<String> = Vec::new();
    if t.fail_ms * 10 > (t.ok_ms + t.fail_ms) && t.fail_ms > 0 {
        flags.push(format!(
            "失败调用（超时 {}、限流 {}、欠费 {}、错误 {}）白等了 {}，占模型耗时 {}：检查超时阈值 / 并发是否顶到限流 / 模型链里有没有已欠费的模型",
            t.timeout,
            t.rate_limited,
            t.billing,
            t.error,
            secs(t.fail_ms),
            pct(Some(t.fail_ms as f64 / (t.ok_ms + t.fail_ms) as f64))
        ));
    }
    if t.truncated > 0 {
        flags.push(format!("{} 次调用输出被长度上限截断，产出是半截的（要续写或重做）", t.truncated));
    }
    for (name, a) in &r.by_step {
        if let (Some(h), true) = (a.cache_hit(), a.input >= 200_000) {
            if h < 0.5 {
                flags.push(format!(
                    "步骤 {name}：输入 {} 但缓存命中只有 {}——重传的历史没吃到缓存",
                    n(a.input),
                    pct(Some(h))
                ));
            }
        }
        if let (Some(x), true) = (a.amplification(), a.input >= 500_000) {
            if x > 100.0 {
                flags.push(format!(
                    "步骤 {name}：输入是输出的 {}，工具循环每轮都在重传全部历史；考虑批量读（`read(paths=[…])`）减少轮数",
                    ratio(Some(x))
                ));
            }
        }
    }
    if t.turn_totals_only {
        flags.push("带 * 的行只有 turn 合计（旧日志没有逐次用量）：看不出每轮重传，也没有失败调用的 token".into());
    }
    let _ = writeln!(out, "\n── 浪费信号 ──");
    if flags.is_empty() {
        let _ = writeln!(out, "（没有明显异常）");
    }
    for f in flags {
        let _ = writeln!(out, "- {f}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(model: &str, ms: u64, fin: &str, i: u64, o: u64, c: u64) -> Value {
        json!({"ModelCall": {"model_id": model, "latency_ms": ms, "finish_reason": fin,
               "usage": {"input_tokens": i, "output_tokens": o, "cache_read_tokens": c}}})
    }

    #[test]
    fn classifies_finish_reasons() {
        assert_eq!(CallClass::of("tool_calls"), CallClass::Ok);
        assert_eq!(CallClass::of("stop"), CallClass::Ok);
        assert_eq!(CallClass::of("length"), CallClass::Truncated);
        assert_eq!(CallClass::of("timeout"), CallClass::Timeout);
        assert_eq!(CallClass::of("error: all models unavailable … API error: 429 - rate_limit_error"), CallClass::RateLimited);
        assert_eq!(CallClass::of("error: … 余额不足/配额耗尽 … 402"), CallClass::Billing);
        assert_eq!(CallClass::of("error: boom"), CallClass::Error);
    }

    #[test]
    fn step_comes_from_role_started_detail() {
        let main = vec![
            json!({"type": "RoleStarted", "sub_id": "tutor-1", "detail": "workflow step 'ch_logic'"}),
            json!({"type": "RoleStarted", "sub_id": "tutor-2", "detail": "workflow step 'plan'"}),
            json!({"type": "RoleStarted", "sub_id": "x", "detail": "no step here"}),
        ];
        let m = step_map(&main);
        assert_eq!(m.get("tutor-1").map(String::as_str), Some("ch_logic"));
        assert_eq!(m.get("tutor-2").map(String::as_str), Some("plan"));
        assert!(!m.contains_key("x"));
    }

    #[test]
    fn aggregates_per_call_usage_by_step_role_and_model() {
        let main = vec![json!({"type": "RoleStarted", "sub_id": "tutor-1", "detail": "workflow step 'ch_logic'"})];
        let sub1 = vec![
            json!({"role_name": "tutor", "sub_id": "tutor-1"}),
            call("glm", 5_000, "tool_calls", 10_000, 100, 0),
            call("glm", 6_000, "tool_calls", 20_000, 100, 10_000),
            call("glm", 7_000, "stop", 30_000, 800, 20_000),
            call("glm", 45_000, "timeout", 0, 0, 0),
        ];
        let sub2 = vec![
            json!({"role_name": "advisor"}),
            call("mini", 15_000, "stop", 2_000, 50, 0),
        ];
        let r = analyze(&main, vec![("tutor-1".into(), sub1), ("advisor-9".into(), sub2)]);
        let t = &r.total;
        assert_eq!((t.calls, t.ok, t.timeout), (5, 4, 1));
        assert_eq!((t.input, t.output, t.cache_read), (62_000, 1_050, 30_000));
        assert_eq!(t.fail_ms, 45_000, "超时那次白等了 45s");
        assert_eq!(t.max_input, 30_000);
        let step = &r.by_step["ch_logic"];
        assert_eq!((step.calls, step.input), (4, 60_000));
        assert!(r.by_step.contains_key("(advisor 审查)"), "没有 RoleStarted 的 advisor 单独归类: {:?}", r.by_step.keys());
        assert_eq!(r.by_model["glm"].calls, 4);
        assert_eq!(r.by_model["mini"].input, 2_000);
        // 单次输入 10k → 30k：历史在重传中涨大
        let s = r.subs.iter().find(|s| s.sub_id == "tutor-1").unwrap();
        assert_eq!((s.first_input, s.last_input), (10_000, 30_000));
        assert!((t.cache_hit().unwrap() - 30_000.0 / 62_000.0).abs() < 1e-9);
        assert_eq!(t.fresh_input(), 32_000);
    }

    #[test]
    fn legacy_logs_fall_back_to_turn_totals_and_say_so() {
        let sub = vec![
            json!({"role_name": "tutor"}),
            json!({"ModelCall": {"model_id": "glm", "latency_ms": 5000, "finish_reason": "stop"}}),
            json!({"TurnEnd": {"total_input": 500_000, "total_output": 5_000, "total_cache_read": 100_000}}),
        ];
        let r = analyze(&[], vec![("t-1".into(), sub)]);
        assert!(r.total.turn_totals_only);
        assert_eq!((r.total.input, r.total.output), (500_000, 5_000));
        let text = render(&r);
        assert!(text.contains("tutor*") && text.contains("只有 turn 合计"), "{text}");
    }

    #[test]
    fn render_flags_the_real_waste_signals() {
        let main = vec![json!({"type": "RoleStarted", "sub_id": "t-1", "detail": "workflow step 'ch_frame'"})];
        let mut ev = vec![json!({"role_name": "tutor"})];
        for i in 0..40u64 {
            ev.push(call("glm", 8_000, "tool_calls", 20_000 + i * 5_000, 50, 1_000));
        }
        ev.push(call("glm", 90_000, "timeout", 0, 0, 0));
        ev.push(call("glm", 20_000, "length", 5_000, 4_000, 0));
        let r = analyze(&main, vec![("t-1".into(), ev)]);
        let text = render(&r);
        assert!(text.contains("白等"), "{text}");
        assert!(text.contains("输出被长度上限截断"), "{text}");
        assert!(text.contains("步骤 ch_frame") && text.contains("缓存命中只有"), "{text}");
        assert!(text.contains("工具循环每轮都在重传"), "{text}");
        // 干净的数据不报警
        let clean = analyze(&[], vec![("c".into(), vec![json!({"role_name":"tutor"}), call("m", 1000, "stop", 1000, 500, 900)])]);
        assert!(render(&clean).contains("没有明显异常"));
    }

    #[test]
    fn number_formatting_has_thousands_separators() {
        assert_eq!(n(0), "0");
        assert_eq!(n(999), "999");
        assert_eq!(n(1_000), "1,000");
        assert_eq!(n(20_480_603), "20,480,603");
    }
}
