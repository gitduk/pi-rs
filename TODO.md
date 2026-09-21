# TODO

未排期的改动，和已定但暂缓的方向。做完一条删一条。

## 待做

**crate 边界**

- [ ] **skills 从 `tools` 拆成独立 crate**（未动手，等发话）。
  动机：`crates/tools` 里有 `skill.rs`（199 行，`SkillTool`）和 `skills.rs`（261 行，发现与 `Skill`），与「工具实现」不是一回事。
  目标：新 crate `crates/skills`，`tools` 不再含 skill 代码。

  新 crate `crates/skills`：
  - `src/lib.rs`：发现层——`Skill`、`Found`、`sources`、`discover`、`discover_from`、`frontmatter`、`body`（现 `tools/src/skills.rs` 整体搬入）。
  - `src/load.rs`：`Load`（由 `SkillTool` 改名，见下）、`instructions`、`NAME`（现 `tools/src/skill.rs` 整体搬入）。
  - 依赖：`tools`（`Tool`/`Ctx`/`Tier`/`ToolError`/`ToolOutput`/`parse_args`/`read`）、`brain`（`slice::head_bytes`）、`async-trait`、`serde`、`serde_json`、`serde_yaml_ng`、`tokio`（`fs`）；dev：`tokio`、`tempfile`。

  `crates/tools` 侧：
  - 删 `src/skill.rs`、`src/skills.rs`，删 `lib.rs` 里 `pub mod skill;`/`pub mod skills;`。
  - `read::MAX_BYTES` 和 `read::over_limit` 由 `pub(crate)` 放宽为 `pub`（新 crate 要用，这是 `tools::read` 公开面的扩大）。

  装配与调用点：
  - 根 `Cargo.toml` 加 `skills = { path = "crates/skills" }`；`crates/cli/Cargo.toml` 加 `skills.workspace = true`。
  - `crates/cli/src/main.rs:356` `tools::skills::discover` → `skills::discover`；`:370` `tools::skill::SkillTool::new` → `skills::Load::new`。
  - `crates/cli/src/repl.rs:6` `use tools::skills::Skill;` → `use skills::Skill;`；`:1112` `tools::skill::instructions` → `skills::instructions`；`:1926`（`#[cfg(test)]` 模块里）同一句 `use` 也要改。
  - 测试 `crates/tools/tests/skill.rs` → `crates/skills/tests/skills.rs`（`tools/tests/common` 跨 crate 用不了，需自带一个小 `ctx()`；里面一处 `SkillTool::new` 同步改名）。

  验证：`cargo build`、`cargo test`、`cargo clippy --all-targets`（预期静默）、提交前 `cargo fmt`。
  无环：`tools::Registry::builtin()` 本来就不含 skill 工具（注释已说明「`task` 和 `skill` 由能构造它们的一方事后挂上」），所以 `skills → tools` 单向，`tools` 永不反向依赖 `skills`。

  顺带改名：**`SkillTool` → `Load`**。理由：全库 `impl Tool for` 的类型名都是裸名动作（`Read`/`Write`/`Edit`/`Grep`/`Glob`/`Bash`/`Fetch`/`Judge`/`Task`），带 `Tool` 后缀的只有 `SkillTool` 和 `ScriptTool`；而且搬进 `crates/skills` 后会读成 `skills::SkillTool`。
  `Load` 合这个模式，也正是工具描述的首句「Load a skill's instructions and follow them.」。wire name 保持 `skill` 不变（它在 `compact.rs` 的 `PROTECTED` 白名单里，不跟 struct 名挂钩——`ScriptTool::name()` 也是返回脚本自己的名字）。
  改动面：仅定义（`skill.rs:24`）、`main.rs:370`、`tests/skill.rs:29` 三处。

- [ ] **`Task`（子代理）从 `agent` 拆成 `crates/task`**（未动手，等发话）。细节见下面「工具实现按域分家」。

- [ ] **`ScriptTool` 拆成 `crates/scripts` 并改名 `Script`**（未动手，等发话）。细节见下面「工具实现按域分家」。

- [x] **`hashline` 和 `syntax` 并回 `tools`**（**已做**）。细节见下面「hashline 与 syntax 并回 `tools`」。

**改名与清理**

- [ ] **`brain` → `llm` 改名**（已定名，暂不替换）。
  范围：`brain::` 178 行 / 195 处、25 个文件，加 4 个 `Cargo.toml`（根 `Cargo.toml` 的 workspace dep，和 `crates/{agent,cli,tools}/Cargo.toml`）。
  两处要手动改：`crates/cli/src/journal.rs:391` 的 crate 白名单（`MINE` 硬编码了 workspace 里所有 crate 名，按 `journal.rs:386-389` 的注释，漏一个就会让那个 crate 的 `tracing` 调用在每一级静默丢掉）；`crates/agent/src/event.rs:2` 注释「the type now lives in `brain`」。

  **这张名单今天就已经错了**，跟本次改动无关：workspace 的 package 名是 `pi`/`agent`/`brain`/`tools`/`hashline`/`syntax`/`wechat`，而 `"cli"` **不是任何 crate 名**（`crates/cli` 的 package 叫 `pi`），`wechat` 则漏了。实测在用到的 target 只有 `pi::*` 和 `tools::grep`（外加 `hyper::pool`），所以这两处错至今没暴露。
  顺带同步 `README.md:375` 的 Layout 表。
  纯内部改名，对外行为不变。

  注意 `MINE` 这张名单被好几个条目同时动：`brain`→`llm` 改一项，`hashline`/`syntax` 并入删两项，新拆出的 `skills`/`scripts`/`task` 各加一项。做这批改动时最后统一收一次。

- [ ] **`Repl` 拆成 `App` + `Lanes` + `Settings`**（**部分已做**：`App`/`Settings` 已落地，`Lanes` 未拆——原因见「`Repl` 拆成三块」）。

- [ ] **统一 turn/round 的口径**（未动手，等发话）：`loop_max_turns` → `loop_max_rounds`、`lane::Turn` → `Run`、删 `Segment::Turns`、修几处散文。细节见下面「turn 与 round：两个层级」。

## 目标目录结构（未落地）

分层后长这样。`↔` 标的是由现有文件改名或拆分而来。

```
crates/
├── llm/        ↔ brain：wire 类型、传输、SSE、token 估算、消息
├── tools/      契约 + 文件系统域：Tool/Registry/Ctx/Tier、read/write/edit/grep/glob、bash、fetch、judge，加 spill/state/walk/workspace/output/parses/rows/blocks（rows/blocks 是 hashline 的落点）、edit/（编辑引擎）、syntax/（tree-sitter）
├── skills/     ↔ 从 tools 拆出：技能发现 + `Load`
├── scripts/    ↔ 从 tools 拆出：脚本发现 + `Script`
├── task/       ↔ 从 agent 拆出：子代理工具
├── agent/      核心：纯循环 + 接缝
├── cli/        应用：输入 / 驱动 / 存储 / 界面
└── wechat/    不动
```

依赖方向单向收紧：`cli → {agent, scripts, skills, task, tools} → llm`，`llm` 不认识任何人；`skills → tools`、`scripts → tools`、`task → agent`，反向永远不成立。

```
crates/agent/src/            ★ 核心
├── lib.rs          循环的八个动作、Agent、budget 算术、leashed
├── session.rs      transcript、投影、压缩记录
├── context.rs      ✓ 已从 cli 上移：standing 提示词的拼装
├── event.rs        只装事实：Usage / ToolCall / Entry
├── seams.rs        Transport、Approver、Steer（**已落**；`Compactor` 还不是 trait，见下）
└── ext/            挂在接缝上的策略，不在循环体里（**已落**）
    ├── compact.rs  summarize.rs  oneshot.rs
    └── approval.rs retry.rs

crates/cli/src/              ✧ 应用
├── main.rs         启动装配，唯一入口
├── input/          ★ 扩展层：一行文本 → 对核心的调用
│   ├── mod.rs      Intent / Step / read / expand
│   ├── commands.rs Command、内建表、技能并入（原 repl.rs 的一部分）
│   └── complete.rs ↔ tui/complete.rs
├── run/            驱动：编排核心的多次运行
│   ├── mod.rs      App（状态根 + `dispatch`；原 `Repl`）
│   ├── lanes.rs    Lanes（lane 的增删改查；原 `Repl` 的一部分）
│   ├── lane.rs     Lane / Run / Handback（不含 View；`Turn` → `Run` 见下）
│   ├── looping.rs  ↔ 从 lane.rs 拆出：/loop 状态机（`loop` 是关键字）
│   ├── meter.rs    ↔ 从 status.rs 拆出：Tally / Snapshot + cost 计算
│   ├── worktree.rs bash.rs subagent.rs wechat.rs
├── store/          磁盘与配置
│   ├── session.rs  journal.rs
│   ├── config.rs   含 Theme / Segment 等配置数据
│   └── settings.rs ↔ settings.rs
└── ui/             一切面向终端的东西
    ├── render.rs   只剩绘制（文本工具→ store/text.rs，配置类型→ store/theme.rs）
    ├── status.rs   ↔ 只有绘制：Segment 的 render / parts / line（词汇→ store/status.rs）
    ├── line.rs     行模式界面
    └── tui/        mod.rs / editor.rs / panel.rs / row.rs / screen.rs
```

~~`ui/` 里那三个还得再拆~~ **已做**（见「TUI 解耦」一节）：`icons`/`keys`/`text`/`theme`/`status` 都落到了 `store/`，`ui/` 只剩绘制。反向边已清零，方向严格单向：`store` → `input` → `run` → `ui`。

现有文件 → 新位置：

| 现在 | 去 | 动作 |
|---|---|---|
| `agent/{compact,summarize,oneshot,approval}.rs` | `agent/ext/` | **已做**（外加 `lib.rs` 的 `Retry` → `ext/retry.rs`；`Approver`/`Decision`/`Steer` → 新 `seams.rs`） |
| `agent/task.rs`（499 行） | `crates/task/` | **移出 agent**（见下） |
| `agent/event.rs` | `agent/event.rs` | 去掉 `cost` |
| `cli/context.rs`（302 行） | `agent/src/context.rs` | **已做**（见下） |
| `hashline/`（755 行）、`syntax/`（477 行） | `tools/` | **并入**（见下） |
| `cli/repl.rs`（2458 行） | `input/` + `run/` | **已做**（`run/lanes.rs` 未拆，见「`Repl` 拆成三块」；`run/bash.rs` 是新增文件） |
| `cli/lane.rs`（421 行） | `run/lane.rs` + `run/looping.rs` | **已做**（`View` 已剥，`Tally` 另落 `run/meter.rs`） |
| `cli/status.rs`（429 行） | `run/meter.rs` + `ui/status.rs` | **已做**（词汇另落 `store/status.rs`） |
| `cli/tui/complete.rs` | `input/complete.rs` | **已做** |
| `cli/{line,render,keys,icons}.rs`、`cli/tui/*` | `ui/` 下 | **已做**（随后 `icons`/`keys`/`text`/`theme`/`status` 词汇落 `store/`，`ui/` 只剩绘制） |
| `cli/{session,journal,config,settings}.rs` | `store/` | **已做** |
| `cli/{worktree,wechat,subagent}.rs` | `run/` | **已做**（`enter_worktree`/`remove_worktree`/`worktree_listing` 仍留 `App`，同 `Lanes` 的理由） |

`cli/repl.rs`（2458 行）怎么切：`input/mod.rs` 收 `Intent`/`Fate`/`Step`/`Rewound`/`read`/`expand`；`input/commands.rs` 收 `Command`/`Source`/`commands()`（技能并入）/`Choice`/`Candidate`/`complete`；`run/mod.rs` 是 `App`（状态根：`store`/`keys`/`commands` + `lanes: Lanes` + `settings: Settings`，加 `dispatch(intent) -> Step` 与 `fate() -> Fate`）；`run/lanes.rs` 是 `Lanes`；`store/settings.rs` 收 `Settings` 本体（`file`/`claimed` + `reread`/`effective`/`rows`/`claim`/`drop_claim`/`claimed_value`/`file_value` + `mask_secret`，并进现有 `settings.rs`）——`edit`/`revert`/`write_to_file` 是包在它外面的一层，因为要走 `rebuild()` 落到 lane 上，所以留在 `App`；`config`/`args` 是 App 的（一个是解析后的配置，一个是命令行）；`run/bash.rs` 收 `Bashed`/`run_bash`/`bash_said`/`record_bash`（**新文件**，cli 现在没有 `bash.rs`）；`WechatCmd` 归 `input/`（它是 `Step::Wechat` 的载荷，`run/wechat.rs` 反过来从 `input` 取）；`run/worktree.rs` 收 `enter_worktree`/`remove_worktree`/`worktree_listing`（并进现有 `worktree.rs`）。

接线点，落地前先知道：

- `render::Theme`、`status::Segment` 是**配置数据而非界面代码**（`config.rs:94-97` 持有），所以留在 `store/config.rs`，`ui/render.rs` 在上层依赖它。反过来的话 `store → ui` 就成了反向依赖。
- `keys` 眼下挂在 `Lane` 上（`lane.rs` 有 `keys: Arc<Keys>`，`repl.rs` 也读）。按键表是界面词汇，`Lane` 不该有；拆完后 `Keys` 只出现在 `store/config`（产出）和 `ui`（消费）。
- `tools::ToolOutput.spent: Totals`（`tools/src/lib.rs:265`）要跟着 cost 一起走：子代理回报改成 `Usage`，cost 由 `run/meter.rs` 在顶层算。这处会外溢到 `crates/tools`。
- **`cli/context.rs` 上移到 `agent`**（**已做**：`agent/src/context.rs`，`agent::context::*`）。它是 standing 提示词的拼装：`workspace()` 拼 `<workspace path>`，`boundary()` 拼 `<write_paths>`（工作区 + 配置的额外写根，跟 tier 有关），`env()` 拼 `<env date/platform/shell/pi/tier>`，再加 `AGENTS.md` 正文（`context.rs:57-151`），`main.rs:431-447` 把它们追在 system prompt 末尾。三个连带：
  - `env()` 唯一的非-`tools` 外部依赖是 `journal::rfc3339`（`journal.rs:141`，连带 `civil` `:156`）。**已选后者**：`env(stamp, tier)` 收调用方的时钟读数，只取其中的「日」（`split_once('T')`），日历仍留在 cli；「一天而非一刻」这条契约留在 `env` 里，测试用两个相隔一小时的读数断言输出相同。
  - **agent 会第一次读盘**：现在 `crates/agent/src/` 里零 `std::fs`/`tokio::fs`，而 `context::from` 靠 `std::fs::read_to_string` 加祖先目录 walk。这与「核心只留一个循环」有点顶，但 system prompt 本来就是 agent 的东西（`agent::DEFAULT_SYSTEM` 已在里面）。
  - `Setup.standing` / `Resolved.standing`（`agent/lib.rs` 的 `Setup`、`main.rs:322`、`:452`）会变形或消失；但 `Task::new(parent, home, standing)`（`task.rs:96`、`:99`）也吃这个字符串（子代理的 system 是 `{PROMPT}{standing}`），所以 agent 要么留一个 pub 的 standing 取用点，要么继续往下传算好的串。**本次选了「继续往下传」**——形状没动，留待「agent 自己算 standing」那一步。
  - 版本号不用担心：`env!("CARGO_PKG_VERSION")` 在所有 crate 里一样（`version.workspace = true`，lockstep 发布）。
  - cli 仍需要**文件名列表**给 banner（`Resolved.context` → `View::opening`）——`context::load` 已经返回 `Loaded { text, files }`，接口够用。

## 工具实现按域分家（已定）

规则：**自成一体的工具各自一个 crate，同域多动作共用一个。** `tools` 保留契约（`Tool` trait、`Registry`、`Ctx`、`Tier`、`ToolOutput`/`ToolError`）加文件系统域。

现在 `impl Tool for` 的地方只有**两处**（`tools/src` 10 个、`agent/src` 1 个，另有 4 个在 `agent/tests/`），但归属没有规则——这才是混乱的根源：

| 实现 | 现在在哪 | 新家 |
|---|---|---|
| `Read`/`Write`/`Edit`/`Grep`/`Glob` | `tools` | `tools`（不动，同域） |
| `Bash`/`Fetch` | `tools` | `tools`（不动） |
| `SkillTool`（→ `Load`） | `tools` | `skills`（已定，顺带改名） |
| `Task`（子代理） | `agent` | `task`（本次定） |
| `Judge` | `tools` | `tools`（见下） |
| `ScriptTool`（→ `Script`） | `tools` | `scripts`（已定） |

新 crate `crates/scripts`：与 `crates/skills` 对仗。自成一体的脚本发现（`discover_in` + 文件头解析）跟技能发现一样，不该混在文件系统工具里。

- `src/lib.rs`：发现层——`discover_in`、`is_identifier`、文件头解析（现 `tools/src/script.rs` 的上半）。
- `src/script.rs`：`Script`（原 `ScriptTool`，就是这一行改名）。
- 依赖：`tools`（`Ctx`/`Tier`/`Tool`/`ToolError`/`ToolOutput`/`Concurrency`/`output::{Capture,take}`/`Workspace`）、`async-trait`、`serde_json`、`tokio`（`process`/`io-util`）；dev：`tokio`、`tempfile`。
- 接口放宽：`tools::bash::reap`（`bash.rs:27`、`:42`）由 `pub(crate)` 放宽为 `pub`。`output::take` 和 `output::Capture` 已经是 pub，不用动。
- `crates/tools` 侧：删 `src/script.rs`，删 `lib.rs` 里 `pub mod script;`。
- 调用点：`crates/cli/src/main.rs:388` `tools::script::discover_in` → `scripts::discover_in`（`:393` 的 `tools::Tool::name` 不变，trait 仍在 `tools`）。
- 测试：`crates/tools/tests/` 里没有指向 `script.rs` 的（不用搬），但 `script.rs:234-259` **自带一个单测**（`an_inherited_name_is_never_shadowed`，用 `tempfile` 和 `tools::Workspace`）——它跟着走，所以 dev-deps 要 `tempfile`。

新 crate `crates/task`：现 `crates/agent/src/task.rs`（499 行）整体挪出，依赖 `agent`/`tools`/`brain`。连带的接口放宽：

- `agent::STOP_GRACE` 和 `agent::event::say` 由 `pub(crate)` 放宽为 `pub`（`Agent` 的 `system`/`registry`/`task_max_turns` 已经是 pub 字段）。`tools::bash::run`（`bash.rs:132`）本来就 pub，不用动。
- `Agent::hang`（`lib.rs:203`）现在直接构造 `task::Task`，挪出后 agent 构造不了它。要么把 `hang` 改成通用的 `hang(tool: impl Tool)`（那它就是 `Registry::with`，可以删掉），要么整个挪到 cli。倾向前者。
- `Home` 是核心向应用要的端口（`Setup.home: Arc<dyn Home>`），倾 **留在 `agent`**——但它是从**要搬走的那个文件里挖出来**的（定义在 `task.rs:21`），不是原地不动：得把 trait 切出来放 agent（如 `seams.rs`），由 `task` 实现。
- 两个文件跟着走：`crates/agent/tests/task.rs`（20.0K，`:16` 的 `use agent::task::{Home, Task}`；它用 `agent/tests/common` 的 `spec()`，跨 crate 拿不到，得自带一份）与 `crates/agent/prompts/task.md`（1.6K，`task.rs:15` 的 `include_str!("../prompts/task.md")`）。
- cli 侧改路径：`cli/src/subagent.rs:11`、`repl.rs:383`、`repl.rs:2355`（`agent::task::Task::NAME`）。

`Judge` 留 `tools`（已定）：它只依赖 `crate::parse_args` 一处，抽出去几乎零成本，但就一个工具、没有自己的发现或生命周期，按「自成一体」的标准不够格。

### 工具类型命名（已定）

规则：`impl Tool` 的类型名用**裸名，与模块同名**（`read::Read`、`grep::Grep`、`glob::Glob`、`bash::Bash`、`judge::Judge`、`task::Task`），不加 `Tool` 后缀。

现有两个例外，都改：

- **`SkillTool` → `Load`**：随 `skills` 拆分一并做。**不能**改成 `skill::Skill`——发现层的 `Skill` 已占名，会出现 `skill::Skill` 和 `skills::Skill` 两个不同类型只差一个字母。wire name 仍是 `skill`。
- **`ScriptTool` → `Script`**：随拆分落进 `crates/scripts`，路径读作 `scripts::Script`（模块名与类型名同名是这库的常规写法）。全库只有 4 处，全在 `tools/src/script.rs`（定义 `:36`、`discover_in` 返回类型 `:46`、构造 `:72`、`impl Tool for` `:106`），零外部调用点。wire name 不受影响（`name()` 返回脚本文件自己头里的名字）。

struct 名不必等于 wire name，这库里本来就不是：`ScriptTool::name()` 返回的是脚本自己的名字，不叫 "script"。

## hashline 与 syntax 并回 `tools`（已定，**已做**）

两个 crate 都只有 `tools` 一个消费者，都既不是工具也不是契约——是工具底下的一层库。`brain` 同属这个货架，但它有 `agent`/`tools` 两个消费者，且就是「模型侧」，所以不动。

### `hashline`（755 行）

它不是工具，是编辑格式本身，所以按消费者拆两处落：

- **编辑引擎**（`apply`/`Edit`/`Anchor`/`Applied`/`Landed`/`Refusal`，现 `hashline/src/edits.rs` 699 行）→ `tools/src/edit/`，与 `edit` 工具同域。消费者只有 `edit.rs` 一个。
- **`header()` + `view_hash()`** → `tools/src/rows.rs`。它们有 **4 个**消费者（`edit`/`read`/`grep`/`write`），而 `rows.rs` 本来就是「五个视图怎么拼一行地址」的那一个出处。

连带的简化：`Blocks` trait 存在的唯一理由是「把 tree-sitter 挡在 `hashline` 外面」（`hashline/src/lib.rs:24-25`），引擎进 `tools` 后 `edit.rs` 本来就 import `syntax`，这层注入没必要了——`Blocks` 和 `NoBlocks` 一起消失（`NoBlocks` 生产代码零调用，只有 `edits.rs` 自己那 2 个单测在用）。

改动点：

- 删 `crates/hashline/`（含那个**空的** `tests/` 目录）。
- 根 `Cargo.toml` 删 `hashline = { path = "crates/hashline" }`；`crates/tools/Cargo.toml` 删 `hashline.workspace = true`。
- `crates/tools/src/edit.rs`：`use hashline::{...}` 改成本地模块；`:775` 的 `&crate::blocks::TreeSitter` 改成直接函数。
- `crates/tools/src/{read,grep,write}.rs`：`hashline::header`/`hashline::view_hash` → `crate::rows::header`/`view_hash`。
- `crates/tools/src/blocks.rs`：`impl hashline::Blocks for TreeSitter` 拆成普通函数。
- `crates/cli/src/journal.rs:391` 的 crate 白名单去掉 `"hashline"`。
- `README.md:379` 的 Layout 表删掉 `hashline` 那一行。
- 顺带修 `crates/tools/src/rows.rs:5` 那条对不上的注释（它说 `hashline` 解析模型回传的东西，但 `hashline` 只产出、不解析）。

### `syntax`（477 行：`lib.rs` 326 + `lang.rs` 151）

消费者只有 `tools` 一个（`blocks.rs`/`edit.rs`/`parses.rs`），而且零测试，所以不必像 `hashline` 那样按消费者拆，整块落一处：

- → `tools/src/syntax/`（`lib.rs` → `mod.rs`，`lang.rs` 原样）。
- `crates/tools/Cargo.toml`：删 `syntax.workspace = true`，把 8 个 tree-sitter 依赖从 `crates/syntax/Cargo.toml` 搬过来（它们**不在** `[workspace.dependencies]` 里，是字面版本：`tree-sitter` 0.25、`-rust` 0.24、`-python` 0.25、`-javascript` 0.25、`-typescript` 0.23、`-go` 0.25、`-json` 0.24、`-md` 0.5）。
- 根 `Cargo.toml` 删 `syntax = { path = "crates/syntax" }`。
- `crates/tools/src/{blocks,edit,parses}.rs`：`syntax::` → `crate::syntax::`。
- 删 `crates/syntax/`。
- `crates/cli/src/journal.rs:391` 的白名单去掉 `"syntax"`。
- `README.md:380` 的 Layout 表删掉 `syntax` 那一行。

### 代价

`hashline` 的 699 行安全关键代码失去「只有一个依赖的独立 crate」这个编译器保证——并进 `tools` 后跟 tokio/reqwest/tree-sitter 一起编译，也没人拦它做 IO。但这个保证现在也没被用起来：整个 crate 只有 2 个单测，`tests/` 目录是空的。`syntax` 本来就没这个保证可言（零测试）。

## turn 与 round 是两个层级（已定）

先纠正一处：`/loop` 的 round 和 compaction 的 round **不是** agent 的 turn，它们粗一级。`compact.rs:495-500` 把这件事说清楚了，而且是踩过坑之后说清楚的：

> The unit is a round — a prompt and everything that answered it — because **the smaller one was an assistant turn and its results**, which left the question standing with its answer gone.

| 词 | 指什么 | 跨度 |
|---|---|---|
| **turn** | 一次 assistant 回复 + 它那批工具结果 | `agent` 循环的一次迭代 |
| **round** | 一次提问 + 它引出的一切 | **一整个 run，可以含很多 turn** |

证据：`loop_step` 一次 `round += 1` 对应的是一整轮 `agent.steered`（`lane.rs:351`）；compaction 的 drop 单位是 `round_starts`（`compact.rs:448`）；`Entry::Ask.round` 记的是「这是第几个 run」（`session.rs:140-147`）。所以「只留 turn、把 round 那层并进去」等于回到 compaction 试过并退回来的那个划法。

（「turn 是 Anthropic 自己的词」这点已查证：Claude Agent SDK 的 TS 参考里有 `maxTurns` 和 `queued_turn_count`。）

### 真问题：`loop_max_turns` 名不副实

`config.rs:101` 自己的 doc 写的是「How many **rounds** a `/loop` may run」，键名却叫 `loop_max_turns`，`Round::Capped` 比的也是 `looping.round`（`lane.rs:351`）。全库只有这一处**名字和它限的东西不一致**。

### 已定

- **turn** 只指「一次回复 + 工具结果」：`Event::TurnStart`/`TurnEnd`、`Done { turns }`、`max_turns`、`task_max_turns` 都已正确，**不动**。
- **round** 只指「一次提问 + 它引出的一切」：`Looping.round`、`Round` 枚举、`THIN_ROUNDS`、`pending_round`、`Intent::LoopRound`、`Entry::Ask.round`、`compact::round_starts` 都已正确，**不动**。
- **改 `loop_max_turns` → `loop_max_rounds`**：全库唯一名不副实的键。
- **`cli::lane::Turn` → `Run`**：运行状态机，与两层都不同义。
- **删 `Segment::Turns`**。
- **顺带修几处把粗一级写成 turn 的散文**（见下）。

### 改动面

**`loop_max_turns` → `loop_max_rounds`**（用户可见，破坏性；无对应 CLI flag）

- `config.rs:106`、`:109`（字段）、`:180-181`（`default_loop_max_turns`）、`:184`、`:186`（`DEFAULT_LOOP_MAX_TURNS`）、`:227`、`:231`。
- `lane.rs:193` 与 `tui/mod.rs:3117`（那句 `"loop stopped at loop_max_turns ..."`）里的字面量。
- `README.md:192`。
- 旧键**不写兼容**（R47）。

**`cli::lane::Turn` → `Run`**

- 枚举（`lane.rs:25`）、`lane.turn` 字段（`:259`），以及 `turn`/`finish`/`is_running` 里的匹配（`:288`、`:300`、`:311`、`:319`），加全部 `Turn::Running`/`Idle`/`Ended`（`tui/mod.rs` 25 处、`repl.rs` 4 处）。全库没有 `Run`/`Phase` 占名。
- 文档注释同步：`lane.rs:20`、`:258`「Where this lane's turn stands」→ run。
- `tui/mod.rs` 另 4 处 `TurnStart`/`TurnEnd` 是 agent 的事件，属于 turn 那一层，**不随这条动**。

**删 `Segment::Turns`**

- 枚举变体（`status.rs:180`）、render 分支（`:203-204`）、`default_done()` 首位（`:275`），及相应测试。
- **这是用户可见的破坏**：`Segment` 带 `#[serde(rename_all = "snake_case")]`，所以 `"turns"` 今天在 `[status] live/done` 里是合法值；删变体后写过的配置会在启动时报反序列化错。R47 说不管兼容，但别再无声。
- `Snapshot.turns` 仍被 `Segment::InOut` 的判空用到（`status.rs:196`），字段保留不改名；若愿意，把那段判空改成只看 in/out，就能连字段一起删。

**散文里把粗一级叫 turn 的**（只改注释，不动代码）

- `session.rs:140`「`round` numbers a `/loop` **turn**」→ round。
- `lane.rs:82`「A **turn** that did not come from here — a line typed between rounds」→ run。
- `lane.rs:221` 描述借出 session 时长的「a turn」→ run。
- `lane.rs:271`「taken by the **turn** it arms」→ run。

## `Repl` 拆成三块（已定）

它一次干五件事（会话、配置与设置、落盘、给界面读数、派发），所以怎么起名都别扭——不是名字不对，是它太胖。按**状态归属**拆三块：

| 新的 | 拿什么 | 放哪 |
|---|---|---|
| `Settings` | `config`/`args`/`file`/`claimed`，加 `reload`/`adopt`/`rebuilt`/`effective`/`in_force`/`retarget` 与面板的 `setting_rows`/`edit`/`revert`/`write_to_file` | **已做** → `store/settings.rs`（`edit`/`revert`/`write_to_file` 留在 `App`，因为要走 `rebuild()` 落到 lane 上） |
| `Lanes` | `lanes: Vec<Lane>`/`current`，加 `lane`/`lane_mut`/`open_lane`/`remove_lane`/`switch`/`fresh_session`/`adopt_session`/`resume`/`becomes`/`resume_listing`/`worktree_listing`/`enter_worktree`/`remove_worktree`/`save`/`save_lane`/`rewind_to`/`tokens_now*`/`status_lines`/`choices`/`listing` | **未做**。复核结论：这 15 个方法里 **12 个要 `self.store`/`config`/`settings`/`args`**，拆出去是把参数逐个往下传，不是状态切分——`lanes`/`current` 本来就是 `App` 的状态，切出来只是换个写字的地方。同源的 `run/worktree.rs` 那三个方法同理。等发话 |
| `App` | `store`/`keys`/`commands` + `lanes: Lanes` + `settings: Settings`，加 `dispatch(Intent) -> Step` 与 `fate() -> Fate` | **已做** → `run/mod.rs`（`lanes: Vec<Lane>` 仍是字段，`Lanes` 没拆） |

`App` 是状态根，`dispatch` 是它唯一真正的逻辑——匹配 `Intent` 那一处必须看得见全部状态，没有更小的地方可放，这是它存在的理由。

名字的由来与取舍：`Repl` 本是准确的历史名（`2d889e6` 那版 `repl.rs` 用 rustyline，`run()` 是真读-求值-打印循环），今天 R 在 `line.rs`/`tui/editor.rs`、P 在 `tui/`+`render.rs`，只剩「把 `Intent` 变成 `Step`」这一半，所以那个名不能留；`Core` 也不行（它产的是 `Step`，做的是应用的账房不是核心逻辑）；`Sessions` 与 `Session`（transcript）撞；`Run` 已被 `lane::Turn` → `Run` 占去。

顺带：

- `Repl::run(intent) -> Step`（`repl.rs:1371`）→ `dispatch`（它不跑循环）。**已做**。
- `repl.rs:316-318` 的 doc 注释还写单数的「A session and everything that outlives any one turn of it」，而字段是 `lanes: Vec<Lane>`——`Lanes` 的注释要重写。**已做**：`App` 的注释改成了「Everything a run holds that outlives any one turn of it」，`lanes` 字段各自有注释。
- `Lane` 自己挂着 `keys`/`commands`（`lane.rs:267-268`），这两样是 `App` 的，`Lane` 不该有（与「接线点」里那条同源）。**已做**：`Keys` 落到 `store/keys.rs`，`Lane` 上两样都没有了。
- 调用面：`tui/mod.rs` 里对 `core.*` 的触碰 **208 处**（拆完后重测：`lane()` 64、`lanes` 61、`lane_mut()` 28、`current` 16、`config` 12、`store` 6 及其余）；`Repl { .. }` 字面构造随改名变成 `App { .. }`。已与 `repl.rs` 的文件拆分同批做完。

## TUI 解耦：残留与三步（已定）

「彻底」只能指**编译期单向**（`run`/`store`/`input` 不 import 界面，界面 import 它们）——`Event`/`Step`/`Intent` 本身就是耦合，核心不可能不知道界面存在，否则没法把决策交出去。

方案切掉的是唯一一处**类型级**的反向边：全库 `crate::tui` 的引用只有 `lane.rs:18 use crate::tui::View`，`View` 移出 `Lane` 就够了。

### 残留（按拆分后的落点列，不按现在的文件名）

| 落点 | 反向引用 | 原处 |
|---|---|---|
| `run/mod.rs`（`status_lines`、`resume_listing`） | `render::spent`、`render::clip`、`render::pad` | `repl.rs:1322`/`:1769`/`:1785` |
| `run/worktree.rs`（`worktree_listing`） | `render::pad` | `repl.rs:1740` |
| `run/lanes.rs`（`Lane.keys`） | `keys::Keys` | `lane.rs:267` |
| `input/commands.rs`（`gist`、`complete`） | `render::clip` | `repl.rs:131`、`:305` |
| `store/settings.rs`（`open_panel`） | `icons::CHANGED_MARK` | `repl.rs:1476` |
| `run/wechat.rs` | `render::clip`、`render::summarize` | `wechat.rs:223`、`:642` |
| `store/config.rs` | `keys::Keys`（`key_map()`）、`render::Theme`、`status::Segment` | `config.rs:94`、`:97`、`:219`、`:225` |

（`journal.rs` 用 `icons` 只在测试里，不算。）

### 根源：两个混装模块

| 模块 | 行数 | 混了什么 |
|---|---|---|
| `render.rs` | 1399 | 纯文本工具（`visible_width`/`clip`/`pad`/`spent`/`summarize`）+ 配置类型（`Theme`/`Style`/`Color`/`Attr`/`Diff`/`Status`/`Menu`/`Prompt`）+ 绘制（`Escape`/`parse_sgr`/`describe`） |
| `keys.rs` | 1280 | keymap 解析（配置侧，`Keys::resolve`）+ 分发词汇（`Action`/`Press`/`Mode`）+ help 列表格式化 |

各横跨三层，所以放哪一层都会有人反向去取。`icons` 是另一回事——44 行常量、零逻辑，纯粹放错了位置。

### 三步

1. `icons` 下移到 `store/icons.rs` 或顶层 `glyphs.rs`（一次移动，干掉 6 条反向边）。
2. `render.rs` 拆三份：文本工具 → `text.rs`；配置类型 → `store/`（`Theme` 本来就按「接线点」那条住那儿）；绘制 → `ui/render.rs`。
3. `keys.rs` 拆三份：keymap 解析 → `store/`（配置的产出）；`Action`/`Press`/`Mode` → `input/`；help 列表格式化 → `ui/`。

### 有意留的一条

`Step` 的载荷是拼好的字符串（`Step::Handled(Vec<String>)`、`Step::Flash(String)`），`status_lines` 也是核心在拼面向用户的文本。要彻底就得让 `Step` 带数据、界面自己排版——更大的改动，**有意不做**，写在这儿免得以后当成漏掉的。

## 已定方向（未排期）

核心是「一个 agent 循环」，其余全部上移或外挂。

- 核心只留八步：投影 → 组装 `Request` → 出网 → 回写 assistant → 判终止 → 跑工具 → 回写结果 → 回投影。
- `cost` 出核心：`spec.cost()` 不参与控制流，乘法挪到调用方，`agent::Event` 去掉 `cost` 字段，`Agent::run` 返回 `Usage`；`tools::ToolOutput.spent` 跟着改成 `Usage`。
- `/loop` 出核心：`Looping`/`Round`（现 `crates/cli/src/lane.rs`）和 TUI 的 `step_loop` 归到核心**上面**的驱动器，核心不知情。
- 命令解析出核心：`read`/`expand`/skills 收进输入层，不在核心。
- `Intent` 三分：命令意图 / 队列专用（`Submit`、`LoopRound`）/ UI 私有（`None`、`Interrupt`、`Rewind`、`Setting*`）。
- 压缩这道接缝是**承重**的：它必须在请求前跑（transcript 装不下 = 整轮没了），默认实现只能是恒等（什么都不删），不能缺席。
- TUI 与核心解耦：`View` 移出 `Lane`（切断 `lane.rs → tui` 的反向依赖）——**已做**（`View` 归界面，按 lane token 存）；`Lane` 裸字段收进只读 `snapshot()` 加核心方法——**未做**（`ui/tui/mod.rs` 仍有约 35 处直接读 `lane.worktree`/`lane.looping`/`lane.ctx`/`lane.turn`/`lane.tally` 等字段）。**不加 `Surface` trait**——核心对外的通道已经齐了（`Event` 出事实、`Step` 出决策、`Steer` 入一句话，`Transport`/`Compactor`/`Approver` 是运行中的接缝），而「开始一轮 / 停止 / 交终端」都是界面自己的决定：`Step::Prompt { send, typed }` 请界面开一轮，`Intent::Interrupt`/`Unsend`/`EditExternally` 根本到不了 `dispatch`。残留与三步见上面「TUI 解耦：残留与三步」（已定）。
- 分层目录：见上面「目标目录结构」。`cli` 内部用**模块**不拆 crate：`input`/`run`/`store`/`ui` 是四个模块 + `pub(crate)` 划边界（拆 crate 不可逆，等边界真稳了再说）。

## 待定

- **`Compactor` 的签名**（名字已定，签名未定）：`compact` 收什么、返回什么。手上的形状是 `(session, budget, tx)` → `Vec<Message>`。
  - 名字的理由：库里本来就说 compaction（`compact.rs`、`Event::Compacted`、`/compact`、`Policy`、`maybe_compact`、`compact_now`）；原提案的 `Furnisher`/`fit` 是多余的同义词。
  - 另两道接缝不用动：`Transport` 不变；`Approver` **已经存在**（`agent/src/approval.rs:14`，`Decision`/`Ceiling` 都在，`Agent::approver` 已是 `Arc<dyn Approver>`，签名 `approve(&self, name, tier, args) -> Decision` 不用重设）——原提案的 `Arbiter` 是白造的词。
