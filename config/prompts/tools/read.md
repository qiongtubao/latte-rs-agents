<instruction>
读取单个文件/目录或批量读取多个相互独立的目标。必须且只能提供 `path` 或 `paths` 之一。

- 单目标：`{"path":"src/lib.rs:20-80"}`。支持 `:start-end`、`:start+count`、`:raw` 等选择器。
- 2–10 个独立目标优先一次调用：`{"paths":["src/a.rs:20-80","include/b.h","src/c.rs:raw"]}`。仅当下一个目标依赖本次结果时才分轮读取。
- 批量读取内部并发、按输入顺序归并；单项失败只进入 `failed`，不影响其他结果。
- 批量返回形状为 `{"files":[...],"failed":[{"path":"...","error":"..."}],"count":N}`；单目标返回形状保持不变。
- 批量总输出预算为 192 KiB，首个成功结果始终保留，后续超预算项进入 `failed`。
- 大文件应使用行范围，避免无目的整文件读取。
</instruction>
