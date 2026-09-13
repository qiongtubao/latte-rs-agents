// 无官方类型包的 markdown-it 插件的极简声明。
declare module "markdown-it-container" {
  import type MarkdownIt from "markdown-it";
  const plugin: (
    md: MarkdownIt,
    name: string,
    options?: {
      validate?: (info: string) => boolean;
      render?: (tokens: { info: string; nesting: number }[], idx: number) => string;
      marker?: string;
    },
  ) => void;
  export default plugin;
}
declare module "markdown-it-footnote" {
  import type MarkdownIt from "markdown-it";
  const plugin: (md: MarkdownIt) => void;
  export default plugin;
}
declare module "markdown-it-deflist" {
  import type MarkdownIt from "markdown-it";
  const plugin: (md: MarkdownIt) => void;
  export default plugin;
}
declare module "markdown-it-texmath" {
  import type MarkdownIt from "markdown-it";
  const plugin: (md: MarkdownIt, options?: { engine?: unknown; delimiters?: string }) => void;
  export default plugin;
}
