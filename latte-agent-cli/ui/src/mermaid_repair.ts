// 模型手写 mermaid 的常见语法错误修补。只在原图源渲染失败后才用（成功的图
// 一个字节都不碰），修补后再试一次；仍失败就回退成代码块并显示报错。
//
// 规则全部取自 jemalloc 会话里实测渲染失败的图源（602 个不重复图里 30 个
// 失败），只覆盖 flowchart/graph —— 其它图类型的失败太零散，不值得猜：
//   1. 未加引号的节点文字含 ( ) [ ] { } # 等：A[调用 f(x, y)]、
//      D[arena->bins[ind]]、R2[#[ignore] 5 个] → A["…"]
//   2. 未加引号的连线文字含特殊字符：-->|base_alloc (src/base.c:485)|、
//      -->|@role line| → -->|"…"|
//   3. subgraph 标题不是合法 id：subgraph 面向用户（低频）
//      → subgraph sg1["面向用户（低频）"]
//   4. 虚线带文字的混写：A -- "x" .-> B → A -. "x" .-> B
//   5. 缺图类型头（第一行直接是节点/连线）→ 补 flowchart TD

const DIAGRAM_HEADER =
  /^(graph|flowchart|sequenceDiagram|classDiagram|stateDiagram|erDiagram|gantt|pie|journey|gitGraph|mindmap|timeline|quadrantChart|sankey|xychart|block|packet|architecture|requirementDiagram|C4\w*|kanban|radar|treemap|zenuml|---|%%)/;

/** 节点文字里出现这些字符时，不加引号 mermaid 会把它当语法。 */
const NODE_SPECIAL = /[()[\]{}|#;@"]/;
/** 连线文字里出现这些字符时同理（含 : 与 <>，`|a<b|` 会被当成箭头）。 */
const EDGE_SPECIAL = /[()[\]{}#;:@<>&"]/;

const OPEN_TO_CLOSE: Record<string, string> = { "[": "]", "(": ")", "{": "}" };

function quote(text: string): string {
  return `"${text.trim().replace(/"/g, "#quot;")}"`;
}

/** 规则 1：逐字符扫描（节点文字里可能嵌套同种括号，正则配不平）。 */
function quoteNodeLabels(line: string): string {
  let out = "";
  let i = 0;
  while (i < line.length) {
    if (line[i] === '"') {
      // 已加引号的文字原样跳过（里面的括号不是语法）。
      const end = line.indexOf('"', i + 1);
      const stop = end < 0 ? line.length : end + 1;
      out += line.slice(i, stop);
      i = stop;
      continue;
    }
    const m = /^[A-Za-z0-9_\u4e00-\u9fff]+/.exec(line.slice(i));
    const prev = i === 0 ? " " : line[i - 1];
    if (!m || !/[\s&>|;-]/.test(prev) && i !== 0) {
      out += line[i++];
      continue;
    }
    const idEnd = i + m[0].length;
    const open = line[idEnd];
    const close = OPEN_TO_CLOSE[open ?? ""];
    const next = line[idEnd + 1] ?? "";
    // 已加引号或是复合形状（[[ ]]、[( )]、(( ))、{{ }}、[/ /] …）不动。
    if (!close || next === '"' || /[[(/\\{>]/.test(next)) {
      out += line.slice(i, idEnd);
      i = idEnd;
      continue;
    }
    let depth = 0;
    let j = idEnd;
    for (; j < line.length; j++) {
      if (line[j] === open) depth++;
      else if (line[j] === close && --depth === 0) break;
    }
    if (j >= line.length) {
      // 括号不配对：不臆造结构，原样保留。
      out += line.slice(i);
      break;
    }
    const inner = line.slice(idEnd + 1, j);
    out += line.slice(i, idEnd + 1) + (NODE_SPECIAL.test(inner) ? quote(inner) : inner) + close;
    i = j + 1;
  }
  return out;
}

/** 修补 flowchart 图源。返回 null 表示没有可修的地方（不必重试）。 */
export function repairMermaidSource(source: string): string | null {
  const lines = source.replace(/\r\n?/g, "\n").split("\n");
  const firstIdx = lines.findIndex((l) => l.trim() !== "");
  if (firstIdx < 0) return null;
  const first = lines[firstIdx].trim();

  let header = "";
  if (!DIAGRAM_HEADER.test(first)) {
    // 规则 5：只在第一行就是 flowchart 节点/连线语句时补头，否则不猜
    // （图源位置上可能是一段散文，硬补头只会换一种报错）。
    if (!/^[A-Za-z0-9_]+\s*(\[|\(|\{|-->|---|-\.|==>)/.test(first)) return null;
    header = "flowchart TD";
  } else if (!/^(graph|flowchart)\b/.test(first)) {
    return null;
  }

  let sg = 0;
  const fixed = lines.map((raw, idx) => {
    if (idx === firstIdx && !header) return raw;
    const indent = /^\s*/.exec(raw)![0];
    let line = raw.slice(indent.length);
    if (line === "" || line.startsWith("%%")) return raw;
    if (/^(style|classDef|class|click|linkStyle|end)\b/.test(line)) return raw;

    const sub = /^subgraph\s+(.+?)\s*$/.exec(line);
    if (sub) {
      const title = sub[1];
      // 规则 3：合法形式（id、id["…"]、id[…]、"…"）不动，其余整体当标题。
      if (/^[A-Za-z0-9_\u4e00-\u9fff-]+$/.test(title) || /^[A-Za-z0-9_]+\s*\[/.test(title) || /^".*"$/.test(title)) {
        line = `subgraph ${quoteNodeLabels(title)}`;
      } else {
        line = `subgraph sg${++sg}[${quote(title)}]`;
      }
      return indent + line;
    }

    // 规则 4：虚线/实线带文字的各种混写统一成 mermaid 认的形式
    //   A -- "x" .-> B      → A -. "x" .-> B
    //   A -.-> "x" --> B    → A -. "x" .-> B
    //   A --> "x" --> B     → A -- "x" --> B
    //   A -.废弃的 "x" .-> B → A -. "废弃的 x" .-> B
    line = line.replace(/--\s*("[^"\n]*")\s*\.->/g, "-. $1 .->");
    line = line.replace(/-\.->\s*("[^"\n]*")\s*-->/g, "-. $1 .->");
    line = line.replace(/-->\s*("[^"\n]*")\s*-->/g, "-- $1 -->");
    line = line.replace(/-\.(?!-)([^.\n]*?"[^\n]*?)\s*\.->/g, (_all, text: string) => `-. ${quote(text.replace(/"/g, " ").replace(/\s+/g, " "))} .->`);
    // 规则 6：标签里的字符串拼接 "a" + "b" → "ab"；引号内的字面 \n → <br/>
    line = line.replace(/"\s*\+\s*"/g, "");
    line = line.replace(/"[^"\n]*"/g, (s) => s.replace(/\\n/g, "<br/>"));
    // 规则 7：两条连线挤在一行（A --> B C --> D）→ 拆成两行。
    line = line.replace(
      /^([A-Za-z0-9_]+\s*(?:-->|-\.->|==>)\s*[A-Za-z0-9_]+)\s+(?=[A-Za-z0-9_]+\s*(?:-->|-\.->|==>))/g,
      `$1\n${indent}`,
    );
    // 规则 2
    line = line.replace(/\|([^|"\n]+)\|/g, (all, text: string) => (EDGE_SPECIAL.test(text) ? `|${quote(text)}|` : all));
    // 规则 1
    line = quoteNodeLabels(line);
    return indent + line;
  });

  const out = (header ? `${header}\n` : "") + fixed.join("\n");
  return out === source ? null : out;
}
