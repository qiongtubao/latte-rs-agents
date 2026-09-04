# 工具说明系统（tool docs）

模型能不能用好一个工具，取决于它在 schema 里读到的那段文字。本文档说明这段
文字从哪来、怎么改、改完什么时候生效。

实现：`latte-agent-core/src/tool_docs.rs`（唯一事实来源）+ 工具面板
（`GET/PUT/DELETE /api/tools/:id/doc`）。

## 模型看到什么

```
tool.description
  = 基线描述（Tool::builder 里写死的，编译进代码）
  + "\n\n"
  + 项目/全局 md（.latte/tools.d/<id>.md，模板渲染 + 剥掉标记后）
```

约定沿用 oh-my-pi 的 `prompts/tools/<tool>.md`：**首行是一句话说明用途**，
其后才是 `<instruction>` / `<critical>` / `##` 小节这类细则。面板据此把说明
拆成「简介 + 详情」两栏——首行（或首句）当简介，其余当详情。

`latte` 的内置描述有不少写成一整行（`"读取文件内容。支持行范围选择器：…"`），
所以首行超过 60 字符时会再按第一个句末标点断一次。整段找不到断点就全当简介，
不做硬截断。

## 文档存在哪

| 层 | 路径 | 用途 |
|---|---|---|
| 项目 | `<cwd>/.latte/tools.d/<id>.md` | 优先级最高，跟着仓库走 |
| 全局 | `$LATTE_HOME/tools.d/<id>.md`（默认 `~/.latte/tools.d/`） | 跨项目共用 |
| 别名组 | 同上，但文件名是别名（`mcp.md` / `playwright.md` / `git.md`） | 对组内所有注册名生效 |

`<id>` 必须是**注册名**（模型 schema 里的工具名）。别名组存在是因为角色配置里
写的是 `tools = ["mcp"]`，而注册名是 `mcp_connect` / `mcp_list` / `mcp_call`；
工具自己的 md 优先于别名组的 md。

分层语义与 models / roles 面板一致：**项目层覆盖全局层**，两层可以并存。工具
面板的详情弹层里有一条分层条（`项目层：生效中` / `全局层：被覆盖`），编辑时给
「保存到项目 / 保存到全局」两个按钮，默认指向**当前生效的那层**——正在看全局
文档时点保存就是改那份全局文档，不会悄悄新建一份项目副本。删除也按层给按钮，
删掉项目层会回落到全局层。

> 换到别的工作目录（比如拿 latte-agent 去跑另一个仓库）时，项目层自然是空的。
> 想让工具说明跟着走，就把它们保存到**全局层**（`~/.latte/tools.d/`）——两层都
> 没有的话没有编译期兜底，模型只能看到基线描述。

文件格式（面板保存时自动生成，手写也可以）：

```markdown
<!-- SUMMARY -->
一句话简介。
<!-- /SUMMARY -->

<!-- DETAILS -->
<instruction>
- 什么时候用、参数怎么给
</instruction>
<!-- /DETAILS -->
```

标记是给面板分栏用的，下发给模型前会剥掉。**没有标记的 md 整份算详情**，
所以从别处拷一份 md 直接放进来也能用。

## 模板语法

文档可以按当前会话真实可用的工具改写内容（oh-my-pi 用 handlebars 做同样的事）：

| 语法 | 说明 |
|---|---|
| `{{CWD}}` | 变量替换。未定义 → 空串 |
| `{{#if has_eval}}A{{else}}B{{/if}}` | 布尔分支。未定义的开关按 `false` |
| `{{#unless is_windows}}A{{/unless}}` | 取反分支，同样支持 `{{else}}` |

可嵌套。键名**大小写与下划线都不敏感**：`hasEval` / `HAS_EVAL` / `has_eval` 等价。
未闭合的块按「到文末」处理，不会 panic、不会吞掉整份文档。

内置上下文：

- 开关：每个已注册工具一个 `has_<注册名>`（`has_read` / `has_code_graph` …）、
  每个别名一个 `has_<别名>`（组内任一成员在就为真）、`has_mcp_tools`、
  `is_windows` / `is_macos` / `is_linux`；
- 变量：`cwd`、`os`、`tools`（逗号分隔的工具清单）、`tool_count`、
  `tool`（当前工具自己的名字）、`doc_path`。

渲染上下文按**每个会话实际注册的工具集**构造，所以同一份 md 给 programmer 和
给 reviewer 看到的内容可以不同。面板里的「渲染预览」用的是当前进程可见的全部
工具，只是预览。

## 改完什么时候生效

保存（`PUT`）或删除（`DELETE`）之后，后端会把**所有活着的 `ToolManager`**
重新 enrich 一遍，返回体里的 `refreshed_managers` 就是刷新了几个会话——
进行中的会话不用重开。

之所以能重复执行不出错：enrich 是 `基线描述 + 当前文档` 的纯函数
（`tool_docs::base_description` 记住第一次见到的原始描述），不是往描述后面
不断追加。重写注册项走的是 `ToolManager::replace`（注册表内部一次写锁），
不会出现「工具短暂不存在」被并发 turn 撞上的窗口。

## 四类工具

| 分类 | 来源 | 能写文档吗 |
|---|---|---|
| `builtin` | tools crate 的 builtin package + core 单独注册的 `code_graph` | 能 |
| `dynamic` | controller 运行时注册（`delegate` / `workflow` / `ask` / `plan` / `task_report` / `request_tool` / `generate_image` / 4 个 `doc_*`），见 `DYNAMIC_TOOL_CATALOG` | 能 |
| `package_alias` | 配置层别名 `git` / `mcp` / `playwright`，见 `TOOL_ALIAS_GROUPS` | 能，对组内所有工具生效 |
| `mcp` | 连上外部 MCP server 后发现的工具 | 能 |

新增一个 `dynamic` 工具时，**必须同时往 `DYNAMIC_TOOL_CATALOG` 里加一条**，
否则它不会出现在工具面板与角色编辑器的工具选择器里（这两处都从这张表取）。
表里的名字要与 `Tool::builder` 的第一个参数逐字一致——文档文件名按它查。

## 外部 MCP 工具

`mcp_connect` 连上 server 后：

1. 发现的工具记进 `tool_docs` 的 MCP 目录（工具面板据此列出，`kind = "mcp"`）；
2. 每个工具**注册成一等工具**，参数 schema 从 MCP 的 `inputSchema` 搬过来，
   handler 转发到 `mcp_call`。模型直接 `weather_now {city: "上海"}` 即可，
   不必手写 `mcp_call {tool: "weather_now", …}`；
3. 因为它们是真正的注册项，`.latte/tools.d/<工具名>.md` 对它们同样生效。

工具名与已有工具撞名时自动加 server 下标后缀（`read@1`），不覆盖内置工具。

工具面板的「测试」按钮走的就是运行时那套 tool manager，所以可以直接用它连
MCP server：`tool_id = mcp_connect`、`args = {"command": "npx …"}`，连上之后
刷新面板就能看到外部工具。
