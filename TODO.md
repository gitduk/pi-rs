# TODO

下次优化的两个方向（对比 deepseek-harness 的借鉴结论，见仓库历史分析）。

## 1. Spill：超大工具输出落盘 + 定位符

**目标**：超大 bash/read 输出不整段进 transcript，由运行时落盘、返回 opaque locator + retrievalHint（告诉模型怎么读回），而不是先塞进上下文、等压缩时才丢。

**现状**：
- `crates/tools/src/bash.rs:56` 已有 `spill()` 雏形：stdout/stderr 超 30K 字节时写到 temp dir（`pi-bash-{pid}-{n}.log`）并返回路径。
- `read` 有 `MAX_BYTES` 上限；compact 的 head+tail（`pruned`）是事后补救，spill 是源头拦截。
- 缺口：temp 文件无 session 归属、无 retrieval 指引、`read` 工具受 workspace 门约束可能读不到 spill 文件。

**dsh 的设计要点**（照搬）：
- 落盘归运行时 spill 层，不归模型的 `write` 工具——模型不决定何时 spill，运行时按大小触发。
- 返回 `SpillRef { locator, bytes, retrievalHint }`；locator 是 opaque 字符串，backend 可换（本地路径/URI/键）。
- 存储失败（ENOSPC、权限）必须 loud reject，绝不静默降级。
- 按 session 命名空间隔离（fork/resume 继承旧 locator，新 spill 归子 session）。

**pi 落点**：
- 统一 seam：哪个工具触发（bash/read 输出超阈值）、文件放哪（建议 session 状态目录 `~/.local/state/pi/spill/<session>/`，与 journal 同处；若放 workspace 内需 `.gitignore` 策略）。
- `read` 工具在 workspace 门内放行 spill locator（或让 retrievalHint 指向 `bash cat <path>`）。
- 与 compact 的 head+tail 联动：spill 后 transcript 里只留 head+tail + locator。

## 2. 工具超时结构化错误码

**目标**：工具超时返回带稳定错误码的结构化结果，模型能按码分支（识别"超时"而非撞上别的失败）；目前 bash 超时是散文 `"timed out after {}ms; the command and everything it spawned were killed"`（`crates/tools/src/bash.rs:164-168`）。

**dsh 的设计**：
- 工具在自己的 `ToolDefinition` 声明 `timeoutMs`，包装层给协作式 deadline，超时返回 `Error: tool call timed out after <ms>ms` + 结构化 `TOOL_TIMEOUT` 码。
- 注意：dsh 的 `bash`/`read`/`write`/`edit` 故意不声明超时（协作式信号管不住它们）；pi 的 bash 是进程组 SIGTERM→SIGKILL 硬杀，比 dsh 的协作式强，保留。

**pi 落点**：
- `ToolError`/`ToolOutput` 增加稳定错误码字段（`crates/tools/src`），bash 超时填 `TOOL_TIMEOUT`（可细分 `TREE_KILLED`），输出格式 `Error: ... [code: TOOL_TIMEOUT]`。
- 顺带可让 `grep`（大仓库慢）等工具各自声明超时，复用同一包装。
- 与 `brain/src/fault.rs` 的 429 语义分类同一思路：模型按码分支而非解析散文。
