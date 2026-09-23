# TODO

未排期的改动，和已定但暂缓的方向。做完一条删一条。

## 待做

**接缝**

- [ ] **`Compactor` 成为真正的接缝**：名字已定，签名未定。手上的形状是 `compact(session, budget, tx) -> Vec<Message>`。今天它是 `agent/src/lib.rs` 的两个方法（`maybe_compact`、`compact_now`），不是 trait，也没有「默认实现只能是恒等」这个约定。
  名字的理由：库里本来就说 compaction（`agent/src/ext/compact.rs`、`Event::Compacted`、`/compact`、`Policy`、`maybe_compact`、`compact_now`）；`Furnisher`/`fit` 是多余的同义词。
  另两道接缝不用动：`Transport` 不变；`Approver` 已经存在——trait 在 `agent/src/seams.rs:22`，`Decision` 在 `:13`，`Ceiling` 在 `agent/src/ext/approval.rs:10`，`Agent::approver` 已是 `Arc<dyn Approver>`，签名 `approve(&self, name, tier, args) -> Decision` 不用重设。原提案的 `Arbiter` 是白造的词。
- [ ] **`Intent` 三分**：命令意图 / 队列专用（`Submit`、`LoopRound`）/ UI 私有（`None`、`Interrupt`、`Rewind`、`Setting*`）。今天只有 `Prompt`/`Bash`/`Builtin`/`Other`（`input/mod.rs`），而后三类里的几个其实从来是 `Deed`/`Action`，根本不在 `Intent` 上。
- [ ] **`/loop` 出核心**：`Looping`/`Round`（`app/looping.rs`）与 TUI 的 `step_loop`（`ui/tui/mod.rs`）归到核心之上的驱动器，核心不知情——**今天已经如此**，这条只剩「确认核心确实不问」这一半。
- [ ] **命令解析出核心**：`read`/`expand`/skills 收进输入层——同样已经如此（`input/`），核心不问。
- [ ] **`Step` 带数据，界面自己排版**：今天 `Step::Handled(Vec<String>)`、`Step::Flash(String)` 是核心拼好的字符串，`app/status.rs` 的 `status_lines` 也是。要彻底就得让界面排版——这也是 `store/keys.rs` 那一步能不能做的前提（见下）。

**小账**

- [ ] **`store/keys.rs` 的正文拆一层**（可做可不做）：把「键怎么写下来」——`parse`/`named`/`show`/`listing`，约 140 行——从「这张表是什么」里分出去（`store/keys/` 下的 `mod.rs` + 文本那半）。1352 行里 496 行是测试、425 行是那张 59 条的默认表，所以这是组织问题，不是长度问题。
- [ ] **后台 lane 的消息要不要额外提醒**：按判据它落在自己那条 lane 的屏上（已经这么做），但你在别的 lane 上时唯一的提示是 tab 的颜色（`Mark::Done`/`Failed`，`ui/tui/mod.rs` 的 `refresh_tabs`）。选项是「只上屏」或「上屏 + bar 上一条 flash」。
- [ ] **`Cut::Failed` 要不要带错误文本**（`app/looping.rs` 的 `Cut`）：前台 `close_run` 已经印了 `error {e}`（`ui/tui/job.rs`），loop 行里重复一次最难看；后台 lane 上则只知道「失败了」不知道原因。
- [ ] **答复（`Reply`）不带 lane**：斜杠命令的答复画在 menu 区，所以你中途切了 lane 之后，一条后台命令的答复会弹在新 lane 上。多数命令是当场作答（不存在这个窗口），异步的只有 `/wechat on` 和 `/compact`。
- [ ] **屏行的位置与 run 的条目不对齐**：`Lane::held_screens` 缓冲的行只能落在 run 末尾（run 拿着 transcript，谁也插不进它中间），所以回合中途的警告在 `/resume` 重建后落在答案下方，而现场是在答案上方；同样，后台 lane 的 `say_of` 行（`ui/tui/mod.rs` 的 `reconcile`）写在 `pending` 重放之前，loop 的结论会压在被它判定的那一轮之上。要真按现场排，得让行与事件带同一个序号、重放时归并——值得做的时候再动。

## 已确认不做

免得以后重新提。每条都探过。

- **`Lanes` 类型**（`App` 里那组 checkout 单独成类型）：15 个方法里 12 个要 `store`/`config`/`settings`/`args`，拆出去只是把参数逐个下传，而 `lanes`/`current` 本来就是 `App` 的状态。
- **`Keys`/`commands` 移出 `Lane`**：当初的理由是 `Keys` 住在 `ui` 里、`Lane → ui` 是反向边——那个早没了（`Keys` 在 `store/keys.rs`、`Command` 在 `input/commands.rs`，`app → store/input` 是允许的方向）。今天这两个字段是有意的设计：每个 checkout 有自己的 config 和 skills，`in_force()` 在切换时采纳在 front 那条的（`app/mod.rs`，注释写着「A skill belongs to one tree and not another, and so does a rebound key」）。搬出去就得每次切换重新 resolve。
- **`app/tests.rs`**（`App` 的测试单独一个文件）：全仓 41 个带测试的文件里它是唯一一个独立的，其余 40 个都内联 `#[cfg(test)] mod tests`（`9a31e50` 恢复）。
- **`Action`/`Press`/`Mode` 搬到 `input/`**：会造出 `store → input` 与 `input → store` 的环（`store/keys.rs` 的 `BINDINGS` 与 `store/config.rs` 的 `key_map()` 都要 `Action`，而 `input/commands.rs` 用 `store`），而且 `Action` 的消费者只有 `ui/tui/*` 和 `main.rs`——`app`/`input` 一处都不用。
- **`agent::event::say` 放宽为 `pub`**：当初预判搬 crate 会需要，`subagent` 实际不用（调用点全在 `agent/src/lib.rs`）。

## 已定的判据（不是待做，记在这儿免得再吵）

- **通知的三个去处**：①它是斜杠命令自己的答复吗（`Step::Handled`/`Step::Worktrees`/`Step::Swap` 的 `said`/`Step::Wechat`/`Step::Flash`/`admit` 的拒绝）→ menu（`ui/tui/reply.rs`，只此一类能进）；②不是但值得回看 → 上屏，且只落在它自己 lane 的屏上；③不是也不值得回看，或只有跨 lane 才够得着 → flash。一个前提：上屏的行必须能在 rebuild 里重现——要么它在 transcript 里（`Entry`），要么它是 `Entry::Screen`（只给屏幕读、不进 wire）。
- **已知代价**：tally 行是**造行时冻住的文本**，所以 `/reload` 改了 `status.done` 之后，旧的那些行不会跟着重拼（段列表变了，老行不变）。换来的是它们能在 rebuild 里重现——`ui/status.rs` 那句「kept in the scrollback」从今天起是真的。

