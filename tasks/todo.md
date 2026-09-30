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

- [ ] **Phase 1: 修复 jemalloc LAT-110 的 draft.md 实据硬伤**
  - [ ] 修正 §0.1 与 §1.3 的 pseudo 桥档参数与公式矛盾（统一为 $(lg\_base=3, lg\_delta=3, ndelta=1)$，即 $2^3 + 1 \times 2^3 = 16$）。
  - [ ] 修正 §1.2 几何约束算式矛盾（将 `4 × delta = 2 × base` 修正为 $4 \times delta = 1 \times base = 64$）。
  - [ ] 修正 §3.2 slab_size 迭代 trace 中的荒谬等式（移除 `16384 == 1 × 14336`，按源码单判据如实展现迭代）。
  - [ ] 澄清 §0.3 / §3.1 中 size == 0 边界与 §6 中的流向说明。

- [ ] **Phase 2: 修复 latte-rs-agents 引擎 doc_review_loop 机制缺陷**
  - [ ] **解开死锁开关**：从 `doc_review_loop.toml` 中移除 `loop_no_release_on = "NEEDS_FIX"`，恢复引擎自带的 Advisor 终局语义复核放行能力（耗尽时若无实质缺陷则放行进入下游，不再整流崩溃）。
  - [ ] **引入严重性分级 (Blocker vs Nit)**：
    - 更新 `prompts/proofreader.md` 与 `doc_review_loop.toml` 中的 proofreader 任务要求，区分 `BLOCKER`（事实/算式/逻辑硬伤）与 `NIT`（纯理解门槛/措辞建议）；
    - 仅当存在 `BLOCKER` 时才允许输出 `NEEDS_FIX`；若仅有 `NIT` 则输出 `LOOKS_CLEAR`（附带可选改进建议）。
  - [ ] **强化 Tutor 反驳机制与防过度推导**：
    - 更新 `prompts/tutor.md` 与 writer 任务要求，借鉴 deepseek-harness 与 oh-my-pi 的经验：要求以源码为准，对于硬核实现允许技术反驳（Rebuttal），严禁为了迎合读者编造未经源码验证的算式。
  - [ ] **同步全仓三份配置与提示词**：
    - 同步 `config/workflows/doc_review_loop.toml`、`.latte/workflows.d/doc_review_loop.toml` 与 `~/.latte/workflows.d/doc_review_loop.toml`；
    - 同步 `prompts/` 与 `~/.latte/prompts.d/`。

- [ ] **Phase 3: 回归测试与验证**
  - [ ] 运行 `cargo test` 验证 `learn_loop_nested_doc_review_config_is_hard_gated` 及相关单测。
  - [ ] 更新对应的断言（如移除对 `loop_no_release_on` 的硬断言）。
