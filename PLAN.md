# 计划：优化用户打断（Interrupt）后的上下文注入与终端呈现

## 1. 现状与问题分析

在用户按下 `Esc` 中断当前运行中的工具（例如 `bash cargo check`），随后输入新消息时，界面会额外输出两行提示：

```text
✓ bash cargo --config 'source.crates-io.replace-with="ustc"' check
→ bash cargo check
stopped
▌ 我手动 fetch 了
✗ bash The user stopped this call before it returned; nothing about the call itself failed.
The user stopped the previous run before it finished. Treat the request it was working on as cancelled; the message below is what to act on.
```

### 核心问题
1. **语义错误：将打断视作错误（`is_error: true`）**
   * 代码在 `crates/agent/src/session.rs` 中为未完成的工具调用生成 `ToolResult::error(...)`。
   * 该结果不仅文案自我矛盾（文案明确注明 `nothing about the call itself failed`），而且打上了错误标记后，在 OpenAI 等协议中会被添加 `[tool error]`，容易诱导大模型误以为工具报错而去尝试自动修复或重试。
   * 在 TUI 层面，`is_error` 会渲染为红色的 `✗` 符号，误导用户以为刚才的命令崩溃或失败。

2. **终端输出冗余：第一条提示在 UI 上未静默**
   * 用户打断时，TUI 的 `close_run` 已经明确输出了灰色的 `stopped`。
   * 下一轮发送消息时，`ui.adopt(...)` 又把合成的未完成工具结果渲染输出到屏幕上，导致界面信息严重冗余。
   * 此合成结果仅是为了满足 LLM 协议闭合 `tool_use` 的要求，对用户来说完全属于内部协议实现细节，应当在 UI 上静默。

3. **上下文冗余：多余的 `STOPPED_BY_USER` 便签**
   * 系统在检测到用户打断后，会生成 `STOPPED_BY_USER` 的 `Entry::Note`。
   * 实际上第一条工具中断结果已经交代了中断事实，且大模型完全能够根据用户发送的最新 prompt 推断当前意图，第二条说明没有存在的必要，并且它还会作为 notice 输出在终端上，造成视觉噪音。

---

## 2. 改造目标

1. **协议闭合但语义正确**：未完成工具返回合成结果时，使用非错误状态（`is_error: false`），准确表示「被用户中止」而非「执行出错」。
2. **UI 终端静默**：此类因用户打断而合成的工具结果，在 TUI 渲染层直接过滤（静默），不再向终端重复打印，界面仅保留打断发生时已有的 `stopped`。
3. **移除 `STOPPED_BY_USER`**：用户主动打断（`StopCause::User`）时，彻底停用 `STOPPED_BY_USER` 的 note 注入逻辑，精简大模型上下文，终端也不再显示该提示。

---

## 3. 详细实施方案

### 阶段一：`crates/agent/src/session.rs`（核心逻辑与模型上下文）

1. **调整未完成调用的结果构建**：
   * 将 `ToolResult::error(c.id, c.name, ...)` 改为普通的非错误结果：
     ```rust
     ToolResult::text(
         c.id,
         c.name,
         "The user stopped this call before it returned.",
     )
     ```
   * 确保 `is_error` 为 `false`，不污染模型判定，不在 OpenAI 协议中附加 `[tool error]`。

2. **移除 `STOPPED_BY_USER` 注入**：
   * 检查 `self.interrupted` 处理逻辑：
     ```rust
     if let Some(cause) = self.interrupted.take() {
         match cause {
             StopCause::User => {
                 // 用户主动打断无需向模型注入冗余 note，模型以最新 prompt 为准
             }
             StopCause::Other => {
                 self.push_note(STOPPED_UNKNOWN);
             }
         }
     }
     ```
   * 清理不再需要的 `STOPPED_BY_USER` 常量及其引用。

### 阶段二：`crates/cli/src/tui/mod.rs`（UI 渲染与终端静默）

1. **在 `f_entry` 中对合成打断结果进行静默处理**：
   * 当 entry 为 `LogEntry::Tool` 时，检查其内容是否为系统自动填充的用户中止结果（例如内容包含 `"The user stopped this call"`）。
   * 命中该条件的条目，`f_entry` 返回 `None`，不生成任何渲染行（`Row`）。
   * 使得无论是后续轮次的 `adopt` 还是历史记录重建（rebuild），该条目均对终端用户完全静默。

### 阶段三：测试用例更新与回归

1. **更新 `crates/agent/src/session.rs` 测试用例**：
   * 更新 `a_user_stop_is_named_before_the_next_prompt`：断言用户主动停止后不会向 session 插入 note。
   * 更新 `a_stop_note_is_model_only_and_not_rewindable`：调整或重构为针对 `StopCause::Other` 场景的测试。
   * 确认 `from_messages`、`send_prompt` 等相关测试通过。

2. **更新 `crates/cli/src/tui/mod.rs` 测试用例**：
   * 检查测试 `a_stopped_bash_does_not_tell_the_model_a_request_was_cancelled` 等，确保断言与新行为一致。

---

## 4. 验证步骤

1. 代码格式化：`cargo fmt`
2. 编译与类型检查：`cargo check --all-targets`
3. 单元测试回归：
   * `cargo test -p pi-agent`
   * `cargo test -p pi-cli`
4. 手动场景模拟验证：
   * 启动 `pi`，执行长时间命令（如 `!sleep 10` 或 `bash cargo check`）。
   * 按 `Esc` 打断，确认界面只显示灰色的 `stopped`。
   * 输入新消息，确认界面干净展示新提问，下方不再出现红色的 `✗ bash ...` 和 `The user stopped the previous run...` 文本。
