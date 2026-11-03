# 修复计划：jemalloc LAT-110 算术硬伤修复与 doc_review_loop 审查收敛机制改进

## 一、背景与根本原因
在 jemalloc 的 LAT-110 任务中，`doc_review_loop` 经历了 10 轮拉锯后失败。根本原因包括两层：
1. **任务产物层（实据错误）**：Tutor 为迎合小白审稿员，在 `draft.md` 仓促增补了算式，但引入了荒谬的算术错误（`2² + 1·2⁴ = 16`、`16384 == 14336`、`4×delta = 2×base` 矛盾），导致 Proofreader 抓获硬伤无法通过。
2. **引擎机制层（结构死锁）**：
   - `doc_review_loop.toml` 配置了 `loop_no_release_on = "NEEDS_FIX"`，将审稿员的修改需求误当作致命拒绝（REJECT），直接跳过了引擎内建的 `loop_exhausted_last_resort_review`（Advisor 语义复核放行通道），导致 10 轮一满直接判死整条工作流。
   - Proofreader 缺乏严重性分级（Blocker vs Nit），每次零基础重读都提出大量排版/措辞级琐碎问题。
   - Tutor 缺乏反驳与防幻觉意识，盲目顺从修补导致文档膨胀与错误滋生。

---

## 二、具体执行计划

- [ ] **Phase 1: 修复 jemalloc LAT-110 的 draft.md 实据硬伤**（未开始：产物未定位——`/home/dong/Documents/latte/` 下没有任何项目含 `.latte/learn/`（jemalloc 检出目录无 `.latte/`，latte-rs-agents 也无 `.latte/learn/`）；需确认该 learn 工作流当时在哪个项目目录跑的）
  - [ ] 修正 §0.1 与 §1.3 的 pseudo 桥档参数与公式矛盾（统一为 $(lg\_base=3, lg\_delta=3, ndelta=1)$，即 $2^3 + 1 \times 2^3 = 16$）。
  - [ ] 修正 §1.2 几何约束算式矛盾（将 `4 × delta = 2 × base` 修正为 $4 \times delta = 1 \times base = 64$）。
  - [ ] 修正 §3.2 slab_size 迭代 trace 中的荒谬等式（移除 `16384 == 1 × 14336`，按源码单判据如实展现迭代）。
  - [ ] 澄清 §0.3 / §3.1 中 size == 0 边界与 §6 中的流向说明。

- [x] **Phase 2: 修复 latte-rs-agents 引擎 doc_review_loop 机制缺陷**（4/4 已落地：死锁开关 / 严重性分级 / Tutor 反驳 / 同步配置与提示词）
  - [x] **解开死锁开关**：从 `doc_review_loop.toml` 中移除 `loop_no_release_on = "NEEDS_FIX"`，恢复引擎自带的 Advisor 终局语义复核放行能力（耗尽时若无实质缺陷则放行进入下游，不再整流崩溃）。（已落地）
  - [x] **引入严重性分级 (Blocker vs Nit)**：（已落地）
    - 更新 `prompts/proofreader.md` 与 `doc_review_loop.toml` 中的 proofreader 任务要求，区分 `BLOCKER`（事实/算式/逻辑硬伤）与 `NIT`（纯理解门槛/措辞建议）；
    - 仅当存在 `BLOCKER` 时才允许输出 `NEEDS_FIX`；若仅有 `NIT` 则输出 `LOOKS_CLEAR`（附带可选改进建议）。
  - [x] **强化 Tutor 反驳机制与防过度推导**：（已落地：`prompts/tutor.md` 新增「反驳权（Rebuttal）与防编造」；`doc_review_loop.toml` 的 writer 任务新增 REBUTTAL 通道 + 严禁编造算式；`prompts/proofreader.md` 与 proofread 任务新增「自己读源码复核反驳」规则）
    - 更新 `prompts/tutor.md` 与 writer 任务要求，借鉴 deepseek-harness 与 oh-my-pi 的经验：要求以源码为准，对于硬核实现允许技术反驳（Rebuttal），严禁为了迎合读者编造未经源码验证的算式。
  - [ ] **同步全仓三份配置与提示词**：
    - 同步 `config/workflows/doc_review_loop.toml`、`.latte/workflows.d/doc_review_loop.toml` 与 `~/.latte/workflows.d/doc_review_loop.toml`；（已落地前两份：仓库源 + 本项目运行时拷贝 `.latte/workflows.d/`；`~/.latte/workflows.d/` 本来就没有 doc_review_loop.toml，运行时走项目层，无需同步）
    - 同步 `prompts/` 与 `~/.latte/prompts.d/`。（已落地：`prompts/tutor.md` + `prompts/proofreader.md` 就是本项目 tutor/proofreader 角色的唯一来源——`~/.latte/prompts.d/` 里没有 tutor/proofreader（role.rs 按 prompt_file 先读项目根 <project>/prompts/，再回落全局 prompts.d/）；另修了 `scripts/sync-config.sh` 的权威源：从陈旧的 `config/prompts/` 改为仓库根 `prompts/`，否则目标项目永远拿不到 proofreader.md）

- [x] **Phase 3: 回归测试与验证**
  - [x] 运行相关单测。（已执行：`cargo test -p latte-agent-core --lib doc_review_loop_` 3/3 通过；`cargo test -p latte-agent-core --lib learn_plan` 27/27 通过；`cargo check --workspace --all-targets` 无错）
    - `doc_review_loop_rebuttal_mechanism_is_pinned`：遍历**所有**按章 fan_out 的章节写作阶段（上游把 doc_review_loop 改成「每章 6 个阶段」，不再是单个 write_chapter），断言每阶段都带 REBUTTAL 通道 +「源码是唯一裁判」+「严禁编造算式」；再钉住 `prompts/tutor.md` 与 `prompts/proofreader.md` 的对应规则。
    - `doc_review_loop_proofread_uses_dynamic_budget`：断言 `prompt_token_budget` 全为空（写死的绝对预算会被文档尺寸顶穿）。
    - `learn_loop_nested_doc_review_config_is_hard_gated`：断言外层 `learn_loop.toml` 的 doc_review 步嵌套 `doc_review_loop`、`output_from = "chapters_out"`，内层各阶段由引擎 fan_out 派发、写权限恰好本阶段负责的节。
    - `learn_plan_config_wires_mechanical_gate` / `mechanical_gate_derives_verdict_from_issues` / `rework_feedback_carries_history_only_when_enabled`：钉住机械终审门（gate_from + gate_attach + 收敛度量 + 历史反馈）与出厂 learn_plan 配置的接线。
  - [x] 更新对应的断言。（已落地：REBUTTAL 断言改为遍历所有章节写作阶段；`loop_no_release_on` 相关断言随上游删除 proofread 步一并移除）
