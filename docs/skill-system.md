# Skill 系统文档

> Skill（技能）是 Agent 角色的扩展指令模块。每个角色有一个主 prompt，可附加多个 Skill 来增强特定领域能力。

---

## 设计思路

Skill 是纯文本的 markdown 指令片段，在运行时会**追加到角色的 system prompt 末尾**。这比内嵌所有知识到 prompt 更灵活——不同的角色可以按需加载不同的技能集。

```
       TOML 配置                  Rust 编译时                   运行时
  ┌─────────────────┐     ┌──────────────────────┐     ┌──────────────────┐
  │ roles.manager    │     │ prompts.rs            │     │ Role.resolve()   │
  │   prompt_file    │────▶│ include_str!("...")   │────▶│ system_prompt =  │
  │   skills = [...] │     │ for_role() / for_skill│     │   main_prompt    │
  └─────────────────┘     └──────────────────────┘     │   + skill1        │
                                                        │   + skill2        │
                                                        │   + ...           │
                                                        └──────────────────┘
```

---

## 文件结构

```
latte-rs-agents/
├── prompts/                          # 所有 prompt 和 skill 文件
│   ├── manager.md                    # 角色主 prompt
│   ├── programmer.md                 # 角色主 prompt
│   ├── architect.md                  # 角色主 prompt
│   ├── reviewer.md                   # 角色主 prompt
│   ├── designer.md                   # 角色主 prompt
│   ├── tech_writer.md                # 角色主 prompt
│   ├── devops.md                     # 角色主 prompt
│   ├── security.md                   # 角色主 prompt
│   ├── tester.md                     # 角色主 prompt
│   ├── pm.md                         # 角色主 prompt
│   ├── screenshot_skill.md           # Skill 文件
│   └── ...                           # 其他 skill 文件
├── config/agents/                    # 角色 TOML 配置
│   ├── manager.toml
│   ├── programmer.toml
│   └── ...
└── latte-agent-core/src/
    ├── prompts.rs                    # 编译时嵌入（include_str!）
    ├── role.rs                       # RoleTemplate → Role 解析
    └── config.rs                     # 配置加载与合并
```

---

## 如何定义一个新 Skill

### 第一步：创建 Skill 文件

在 `prompts/` 下创建一个 markdown 文件。文件名惯例：`<name>_skill.md`。

```markdown
# Database Skill — 数据库操作指南

## 工具：query_db

通过 `bash` 工具执行 SQL 查询：

```bash
# MySQL 查询
mysql -h $DB_HOST -u $DB_USER -p$DB_PASS $DB_NAME -e "SELECT * FROM users LIMIT 5"

# PostgreSQL 查询
psql -h $DB_HOST -U $DB_USER -d $DB_NAME -c "SELECT * FROM users LIMIT 5"
```

## 使用场景

1. 数据迁移脚本编写
2. 慢查询分析
3. 数据完整性检查

## 最佳实践

- 始终使用参数化查询（如适用）
- 读取操作优先于写入操作
- 大表查询加 LIMIT
```

### 第二步：注册到 Rust 编译时

打开 `latte-agent-core/src/prompts.rs`，添加一行：

```rust
/// Database query skill
pub const DATABASE_SKILL: &str = include_str!("../../prompts/database_skill.md");
```

然后在 `for_skill()` 函数中添加映射：

```rust
pub fn for_skill(name: &str) -> Option<&'static str> {
    match name {
        "screenshot_skill" => Some(SCREENSHOT_SKILL),
        "database_skill" => Some(DATABASE_SKILL),  // 新增
        _ => None,
    }
}
```

### 第三步：在角色配置中启用

在角色的 TOML 配置中添加 `skills` 字段：

```toml
[roles.programmer]
id = "programmer"
name = "Software Engineer"
category = "execution"
model_tier = "standard"
prompt_file = "prompts/programmer.md"
temperature = 0.3
tools = ["read", "write", "bash", "search", "delegate"]
icon = "💻"
skills = ["screenshot_skill", "database_skill"]  # 启用多个 skill
```

---

## Skill 加载原理

### 加载优先级

```
1. 配置文件的 prompt_file 路径
2. $LATTE_HOME/prompts.d/<basename>      ← 全局覆盖
3. 内建 include_str! 常量                ← 回退
```

对于 skill 文件，加载路径：

```
1. prompts/<skill_name>.md               ← 项目目录
2. $CARGO_MANIFEST_DIR/prompts/<skill>.md ← 编译目录
3. crate::prompts::for_skill(name)        ← 内建回退
```

### Rust 代码流程

```rust
// role.rs — RoleTemplate::resolve()
impl RoleTemplate {
    pub async fn resolve(&self) -> Role {
        let main_prompt = self.resolve_prompt().await;

        // 加载并追加所有 skill
        let mut system_prompt = main_prompt;
        for skill_name in &self.skills {
            // 按优先级查找 skill 内容
            let skill_content = load_skill_from_disk(skill_name)
                .or_else(|| crate::prompts::for_skill(skill_name));

            if let Some(content) = skill_content {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&content);
            }
        }

        Role { system_prompt, ... }
    }
}
```

---

## 内建角色与默认 Skill

| 角色 | 默认 Skill | 说明 |
|---|---|---|
| manager | 无 | 仅 delegate 工具，不做具体实现 |
| programmer | 无 | 可直接自选 skill |
| architect | 无 | — |
| reviewer | 无 | — |
| tester | 无 | — |
| security | 无 | — |
| designer | 无 | — |
| tech_writer | 无 | — |
| devops | 无 | — |
| pm | 无 | — |

注意：内建的 `template_for()`（`prompts.rs` 末尾的 fallback 工厂函数）所有角色默认 `skills = []`。如果要给内建角色加默认 skill，需要修改 factory 函数。

---

## 命令行测试 Skill 加载

```bash
# 查看 manager 角色的完整 system prompt（含已加载的 skill）
latte-agent debug prompt --role manager

# 查看 programmer 角色的完整 system prompt
latte-agent debug prompt --role programmer

# 查看 handlbars 渲染后的 prompt
latte-agent debug prompt --role manager --topic "用户登录模块"

# 查看所有已知角色
latte-agent debug sessions
```

---

## Skill 文件编写规范

### 命名规范

- 文件名：`snake_case_skill.md`
- 一级标题（`#`）：简明描述 skill 用途

### 内容结构

每份 skill 文件建议包含：

| 章节 | 内容 |
|---|---|
| `# 标题` | Skill 名称与一句话描述 |
| `## 工具：<name>` | 所需工具的用法说明 |
| `## 基本用法` | 命令示例 |
| `## 进阶用法` | 复杂场景 |
| `## 使用场景` | 何时启用此 skill |
| `## 最佳实践` | 注意事项 |

### 模板

```markdown
# <Name> Skill — <简短描述>

## 工具：<tool_name>

通过 `<tool>` 工具操作：

```bash
# 示例命令
```

## 使用场景

1. 场景一
2. 场景二

## 最佳实践

- 要点一
- 要点二
```

---

## 全局 Skill 覆盖

用户可以在 `$LATTE_HOME/prompts.d/` 下放置同名文件来覆盖内建 skill：

```bash
# 创建自定义 screenshot_skill（覆盖内建版本）
mkdir -p ~/.latte/prompts.d
cp my_custom_screenshot_skill.md ~/.latte/prompts.d/screenshot_skill.md

# 验证覆盖生效
latte-agent debug prompt --role programmer | grep screenshot
```

---

## 注意事项

1. **性能**：Skill 内容在 resolve 时拼接到 system prompt。过长的 skill 会增加 token 消耗。建议每个 skill 保持在 50 行以内。
2. **冲突**：多个 skill 如果定义相同的工具使用方式，可能导致指令冲突。推荐按职责分离。
3. **编译时 vs 运行时**：内建 skill 通过 `include_str!` 编译到二进制中。这意味着 `cargo build` 后才能看到 prompt 修改的效果。开发时可以用 `$LATTE_HOME/prompts.d/` 覆盖，无需重新编译。
4. **目前不支持 Skill 热加载**：每次 resolve 都是从文件读取（或返回常量引用），但配置和角色在 `ChatController::spawn()` 时确定。如果修改 skill 文件，需要重启聊天会话才能生效。
