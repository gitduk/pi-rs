# /wechat —— 把 pi session 桥到微信

> 交付物：本文件原样落到 `/home/wukaige/pi-rs/WECHAT.md`，交由另一个模型实施。
> **已实施（2026-09，pi 1.0.6）**：`crates/wechat` 协议客户端、`crates/cli/src/wechat.rs` 桥、`/wechat [on|off]` 命令、TUI 接线均已落地，本文件第 3 节的未知项已逐条核实并回填（见 2.9 与第 3 节）。剩余待办：真实微信账号的端到端扫码验证（见第 7 节）。

---

## 1. Context：为什么做这个

pi 是一个只在单个目录内活动的本地 coding agent，当前只有两个 surface：终端 TUI 和无终端的行模式。目标是让运行中的 session 多出一个远程入口——人不在电脑前时，用微信给它发指令、看它的输出。

腾讯 2026 年通过 OpenClaw 开放了微信个人号 Bot API（iLink 协议，域名 `ilinkai.weixin.qq.com`），**HTTP/JSON 长轮询，无 webhook**。这条是决定性的：pi 不需要开公网端口、不需要内网穿透，一个后台 tokio task 就够，与「本地 CLI」的定位不冲突。

**范围**：第一版只做纯文本双向、单会话（桥到当前运行的 session）。不做媒体、不做群聊、不做多 session 分发。

**权限控制明确不做**——账号所有者已判定只有本人能给该 bot 发消息，白名单与降级 Approver 都是多余代码。不要自作主张加回来。

---

## 2. 协议速查

来源：`https://raw.githubusercontent.com/hao-ji-xing/openclaw-weixin/main/weixin-bot-api.md`（逆向 `@tencent-weixin/openclaw-weixin` v1.0.2 得出，非腾讯官方 API 文档）。

### 2.1 请求头（每个请求固定）

```
Content-Type: application/json
AuthorizationType: ilink_bot_token
X-WECHAT-UIN: <base64(随机 uint32 的十进制字符串)>   // 每次请求重新生成，防重放
Authorization: Bearer <bot_token>                     // 登录后才有
```

`X-WECHAT-UIN` 注意是 **先把 u32 格式化成十进制字符串，再 base64**，不是对字节做 base64。

### 2.2 端点表

| Endpoint | Method | 功能 |
|---|---|---|
| `/ilink/bot/get_bot_qrcode` | GET | 获取登录二维码（`?bot_type=3`） |
| `/ilink/bot/get_qrcode_status` | GET | 轮询扫码状态（`?qrcode=xxx`） |
| `/ilink/bot/getupdates` | POST | 长轮询收消息（核心） |
| `/ilink/bot/sendmessage` | POST | 发送消息 |
| `/ilink/bot/getuploadurl` | POST | CDN 预签名上传地址（本期不用） |
| `/ilink/bot/getconfig` | POST | 获取 typing_ticket |
| `/ilink/bot/sendtyping` | POST | 发送「正在输入」 |

### 2.3 登录

```
GET /ilink/bot/get_bot_qrcode?bot_type=3
→ { "qrcode": "...", "qrcode_img_content": "..." }

GET /ilink/bot/get_qrcode_status?qrcode=<qrcode>
→ { "status": "confirmed", "bot_token": "...", "baseurl": "..." }
```

文档给的 demo 是 1 秒一次轮询直到 `status === "confirmed"`。

### 2.4 收消息（长轮询）

```json
POST /ilink/bot/getupdates
{ "get_updates_buf": "<上次游标，首次空串>", "base_info": { "channel_version": "1.0.2" } }
```

服务器 hold 最多 35 秒。响应：

```json
{
  "ret": 0,
  "msgs": [ /* WeixinMessage[] */ ],
  "get_updates_buf": "<新游标>",
  "longpolling_timeout_ms": 35000
}
```

`get_updates_buf` 必须每次更新并持久化，否则重复收消息。文档 demo 的写法是 `buf = resp.get_updates_buf ?? buf`（响应缺字段时保留旧值）。

### 2.5 消息结构

```json
{
  "from_user_id": "o9cq800kum_xxx@im.wechat",
  "to_user_id": "e06c1ceea05e@im.bot",
  "message_type": 1,
  "message_state": 2,
  "context_token": "AARzJWAFAAABAAAAAAAp...",
  "item_list": [ { "type": 1, "text_item": { "text": "你好" } } ]
}
```

- ID 格式：用户 `xxx@im.wechat`，bot `xxx@im.bot`
- `message_type`：**1 = 用户发来，2 = bot 发出**。demo 里 `if (msg.message_type !== 1) continue`，即只处理 1。
- `message_state`：2 = FINISH（完整消息）
- `item_list[].type`：1 文本 / 2 图片 / 3 语音（silk，附转文字）/ 4 文件 / 5 视频
- 文本取值路径：`msg.item_list[0].text_item.text`

### 2.6 发消息

```json
POST /ilink/bot/sendmessage
{
  "msg": {
    "to_user_id": "<= 入站消息的 from_user_id>",
    "message_type": 2,
    "message_state": 2,
    "context_token": "<原样回带入站消息的 context_token>",
    "item_list": [ { "type": 1, "text_item": { "text": "你好！" } } ]
  }
}
```

**`context_token` 必填且必须原样回带**，否则消息不关联到正确的会话窗口。这是文档明说的最大坑点。

### 2.7 媒体（本期不实现，记录备查）

CDN 上所有文件 AES-128-ECB 加密。上传流程：生成随机 AES-128 key → ECB 加密 → `getuploadurl` 拿预签名 URL → PUT 加密文件 → `sendmessage` 带 base64 的 `aes_key` 和 CDN 引用参数。CDN 域名 `https://novac2c.cdn.weixin.qq.com/c2c`。

### 2.8 条款约束（影响设计）

腾讯《微信ClawBot功能使用条款》4.7：腾讯有权决定可连接的第三方 AI 服务类型与**信息收发规模或频率**，并可对内容识别、拦截、阻断。3.2：腾讯只做管道，不存内容。

→ 实现上意味着：**必须假设请求会被限速或阻断**。长轮询循环要有退避重试，`sendmessage` 失败要能在本地终端报出来而不是静默丢失。


### 2.9 与 v2.4.8 实装的差异（已从 npm 包源码核实）

第 3 节的核实对象是 npm 上当前在售的 `@tencent-weixin/openclaw-weixin` **v2.4.8**（本文件其余部分写的是逆向文档的 v1.0.2，两者有出入，以 v2.4.8 为准）：

- 登录二维码接口是 **POST**（`ilink/bot/get_bot_qrcode?bot_type=3`，body 带 `local_token_list`），不是 GET。
- 所有请求带 `iLink-App-Id: bot` 与 `iLink-App-ClientVersion`（版本号编码为 `major<<16|minor<<8|patch`，2.4.8 = 132104）；GET（扫码轮询）只带这两个头，鉴权四件套只在 POST 上。
- `base_info` 含 `channel_version`（即包版本号，不是常量 "1.0.2"）和 `bot_agent`（默认 "OpenClaw"）。
- `context_token` 是**按消息下发、逐字回带**，参考实现按 (bot_id, user_id) 持久化复用；`sendmessage` 的 msg 还带 `client_id` 与可选的 `run_id`。
- 登录轮询会 IDC 重定向（`scaned_but_redirect` → `redirect_host`），登录后所有请求打 confirmed 响应里的 `baseurl`。
- 额外端点 `ilink/bot/msg/notifystart` / `notifystop`（本实现未用）。
---

## 3. 未知项核实结果（2026-09，全部回填）

以下逐条给出结论与出处。协议侧 1–9 全部从 npm 包 `@tencent-weixin/openclaw-weixin` **v2.4.8** 的 `dist/src/` 源码核实（api.js / login-qr.js / monitor.js / inbound.js / send.js / sync-buf.js / config-cache.js / session-guard.js / types.js）；pi 侧 10–16 从本仓库代码核实。未做的只有「真实账号实测」（见第 7 节）。

### 协议侧

1. **`baseurl` 有用。** v2.4.8 的 `apiGetFetch`/`apiPostFetch` 用 `new URL(endpoint, baseUrl)` 拼接，登录 confirmed 响应里的 `baseurl` 就是后续所有请求的 base（`waitForWeixinLogin` 返回 `baseUrl: statusResponse.baseurl`）。登录中还会 IDC 重定向（`scaned_but_redirect` → `redirect_host`）。**已按此实现**：`Client::new(base_url)`，登录成功后把 confirmed 的 `baseurl` 落盘。
2. **status 取值全集。** `wait`（含客户端 35s 超时）、`scaned`、`need_verifycode`（手机显示验证码，需回输）、`verify_code_blocked`、`expired`（刷新二维码，最多 3 次）、`binded_redirect`（已绑到别的客户端）、`scaned_but_redirect`（带 `redirect_host`）、`confirmed`。**已按此实现**；`need_verifycode` 本版明确不支持（LoginView 不带 read_verify_code 时登录报「需要验证码」退出，不挂死）。
3. **错误结构。** 响应带 `ret` 与可选的 `errcode`/`errmsg`；`sendmessage` 只查 `ret`，getupdates 查 `ret` **或** `errcode`。**token 失效码 = -14**（`STALE_TOKEN_ERRCODE`），参考实现暂停会话 1 小时。**已按此实现**：-14 → 清 token + 本地提示「/wechat off 再 on 重新扫码」，桥停止。
4. **`context_token`。** 按消息下发、回复必须逐字回带；参考实现按 (bot_id, user_id) 持久化复用，**没有 TTL 处理**——文档担心的「隔两小时失效」在参考实现里不存在对应逻辑，即按「复用上次入站的 token」处理。**已按此实现**：状态文件存最后一个入站的 `context_token`，sendmessage 原样回带。
5. **`sendtyping`/`getconfig` schema。** `getconfig` body = `{ ilink_user_id, context_token, base_info }` → 响应 `{ ret, typing_ticket }`；`sendtyping` body = `{ ilink_user_id, typing_ticket, status: 1|2, base_info }`（1=开始输入，2=取消）。ticket 按用户缓存、24h 内随机刷新（`WeixinConfigManager`），**无周期性续发逻辑**。**已按此实现**：turn 开始 typing on、turn 结束 typing off，ticket 缓存复用。
6. **消息长度/分片。** 参考实现**没有任何分片**，文本原样放进一个 `text_item` 发出去。**已按此实现**（超长被截断或报错属于服务端行为，未实测；如需分片再改）。
7. **`item_list` 多 text_item。** 参考实现 text 永远单 item（媒体才多 item 且每个 item 单独发一个请求）。**已按此实现**：一次 sendmessage 一个文本 item。
8. **`group_id`。** v2.4.8 的入站解析里没有 group 分支，`ChatType` 恒为 `"direct"`；消息结构含 `seq`/`message_id`/`create_time_ms`/`session_id` 等字段。**已按此实现**：所有 wire 类型不 `deny_unknown_fields`，多出的字段（含未来的 `group_id`）静默忽略，不会炸反序列化。
9. **长轮询行为。** 服务端 hold ≤35s；客户端 35s 超时按**空结果**处理（参考实现的 `getUpdates` 对 AbortError 返回 `{ret:0, msgs:[], get_updates_buf: 旧值}`）。网络错误退避：**2s 重试，连续 3 次后 30s**（`monitor.js` 的 `RETRY_DELAY_MS`/`BACKOFF_DELAY_MS`），非指数退避。服务端可返回 `longpolling_timeout_ms` 调整下次 hold 时长。**已按此实现**。

### pi 代码侧

10. **Config 结构。** `~/.pi/settings.toml` → `Config`，`#[serde(deny_unknown_fields)]`。**结论：不加 `[wechat]` 段**——bot_token/get_updates_buf 是机器状态不是用户设置，放进 `settings.toml` 会被 `deny_unknown_fields` 拒绝，且 buf 每轮更新会频繁重写用户配置文件。
11. **凭据存取。** `crates/cli/src/keys.rs` 是**键盘绑定表**，不是凭据存储——原设计前提错了。`~/.pi/` 的既有机制是 `tools::state::dir()`（sessions/、spill/、history 都走它），状态文件惯例是 pretty JSON（`session.rs` 的 `serde_json::to_vec_pretty`）。**已按此实现**：`~/.pi/wechat.json`（JSON，含 token/base_url/bot_id/peer/get_updates_buf/context_token），tmp+rename 写入。
12. **reqwest。** cli **没有**直接依赖 reqwest（只有 brain 有）。**已按此实现**：`crates/wechat` 依赖 reqwest（workspace dep），cli 依赖 wechat，不重复引。
13. **Repl 字段。** `agent / store / session / id / created / name / keys / config / args / context / commands / file / claimed / ctx`，确认无误。桥不需要往 Repl 加字段——状态全在 `Bridge` 里。
14. **`ui.queued`。** 类型 `Vec<String>`；写入点两处（Bash 内循环、turn 内循环的 `Act::Submit`）；drain 点一处：`Step::Prompt` handler 里每个 turn 结束后 `std::mem::take(&mut self.ui.queued).join("\n")` → `ui.submit` → 作为下一个 prompt。**入站已复用此队列**，竞态按原设计消失。
15. **transport 模式。** `crates/brain/src/transport/*` 各自持有 `reqwest::Client`（`Client::new()`），公共 `exchange()` 管日志与错误。**已按此抄**：wechat 的 `Client` 同构，超时按端点分类（长轮询 35s / 常规 15s / 配置 10s）。
16. **测试惯例。** 各文件底部 inline `#[cfg(test)] mod tests`；workspace **无 HTTP mock 依赖**。**已按此实现**：serde 解析与 `parse("/wechat")` 等纯逻辑测试，无 mock。

---
## 4. pi 侧架构

### 4.1 关键既有事实（已读证实）

- `crates/agent/src/event.rs` —— `Event` 枚举，`#[derive(Debug, Clone, PartialEq)]`。注释明说 *"a renderer consumes these; the loop never writes to a terminal itself"*。**agent 循环与终端已完全解耦，这是桥能成立的前提。**
- `crates/agent/src/approval.rs` —— `Approver` trait + `Ceiling(Tier)`。当前是静态 tier 上限，**没有交互式逐条批准**。所以远程桥不存在「审批往返」这个最难的问题。本期不动这里。
- `crates/cli/src/repl.rs:651` —— `pub enum Cmd`；`:803` —— `pub enum Step`。
- `crates/cli/src/repl.rs:54-93` —— `Command::builtin(...)` 内建命令表（`/new` `/resume` `/name` `/model` `/compact` `/todo` `/cost` `/reload` `/log` `/keys` `/help` `/exit`），补全表由它构建。
- `crates/cli/src/repl.rs:777-789` —— `fn parse` 把首词映射到 `Cmd`。
- `crates/cli/src/repl.rs:859-897` —— `Repl::command` 里 `match cmd`，产出 `Step`。
- `crates/cli/src/line.rs:16` —— `pub async fn run(mut core: Repl, tx: UnboundedSender<Event>)`，无终端 surface。
- `crates/cli/src/tui/mod.rs:1398` —— `Tui::run`；`:1626` 主 `tokio::select!`；`:1628` `Some(event) = rx.recv() => ui.on_event(event)`；紧随其后 `Act::Submit(line) => ui.queued.push(line)`。
- `crates/cli/src/main.rs:510` —— `let (tx, rx) = mpsc::unbounded_channel();`，随后按 stdin/stdout 是否 tty 分派给 `tui::Tui::new(core, key_map)?.run(tx, rx)` 或 `line::run(core, tx)` + `paint(rx, ...)`。

**结论：`Repl` 是纯的，两个 surface 只是消费 `Step` 的两种方式。微信是「第三个输入源 + 第二个 Event 出口」，不是第三个独立 surface。**

### 4.2 分层

```
crates/wechat/                  新 crate，纯协议客户端，不认识 agent
    login()                     二维码 → 轮询 → bot_token
    Client::updates()           长轮询，产出 Inbound
    Client::send(to, token, text)
    Client::typing(to)

crates/cli/src/wechat.rs        桥（bridge），认识 agent 也认识 Client
    Bridge::start()             spawn 长轮询 task，返回 inbound rx + handle
    Bridge::observe(&Event)     攒 delta，turn 结束发出
```

新 crate 只做协议、对外暴露 `recv() -> Inbound` / `send(...)`。桥的逻辑不进 `crates/wechat`，否则这个 crate 就绑死在 pi 上了。

### 4.3 命令接入

按 `Step::Compact` 的既有先例：**需要网络的操作由 surface 执行，`Repl` 只返回意图**（`repl.rs:815` 的注释原话是 *"Needs the network, so the surface runs it and reports"*）。

```rust
// repl.rs:651 enum Cmd 里加
Wechat(String),      // "" = 报状态；"on" = 连接；"off" = 断开

// repl.rs:803 enum Step 里加
Wechat(WechatCmd),   // surface 起停 bridge task
```

要改的匹配点：
- `repl.rs:54-93` 内建命令表加一条 `Command::builtin("/wechat", "[on|off]", "bridge this session to WeChat")`
- `repl.rs:777-789` `parse` 加 `"/wechat" => Cmd::Wechat(rest(line))`
- `repl.rs:859-897` `command` 的 match 加对应 arm
- `line.rs` 的 `match core.command(...)` 加 arm
- `tui/mod.rs` 处理 `Step` 的 match 加 arm

`Cmd` 和 `Step` 都是 `pub enum` 且被穷尽匹配，编译器会把所有该改的点报出来——**不要手工找，加完变体直接 `cargo check` 让它列。**

### 4.4 入站：复用 `ui.queued`

TUI 主循环已经有 `Act::Submit(line) => ui.queued.push(line)`——turn 进行中提交的行排队而非并发。**微信入站消息走同一条队列**，一个 session 两个输入源的竞态自动消失。空闲时直接进 `core.command(line, &totals)`，于是微信端也天然能用 `/todo` `/cost` `/compact` 这些内建命令。

具体做法：`select!` 加第三个分支

```rust
Some(inbound) = wechat_rx.recv() => { /* 与 Act::Submit 相同的处理 */ }
```

⚠️ 需要先确认 `ui.queued` 的 drain 点（未知项 14），保证排队的行在 turn 结束后确实会被取出执行。

### 4.5 出站：在 `ui.on_event` 处 tee

微信发不了流式。策略：

- `TextDelta` / `ReasoningDelta`：只累积，不发。（Reasoning 建议直接丢弃，手机上没价值。）
- `ToolStart`：发一行摘要，如 `⚙ edit crates/cli/src/repl.rs`。**必须限流**——一个 turn 几十个工具调用会把手机刷屏。建议合并成一条、或每 N 秒最多一条。
- `ToolDenied` / `Warning` / `Retrying`：发，这些是用户需要知道的异常。
- `Done`：把累积的文本一次性发出。超长按未知项 6 的结论分片。
- 期间周期性 `sendtyping`（取决于未知项 5）。

`Event` 是 `Clone`，`tui/mod.rs:1628` 的 `ui.on_event(event)` 前后加一行 `bridge.observe(&event)` 即可，不需要改 `tx`/`rx` 的所有权结构。

### 4.6 从微信中断

**本地终端能在工具跑之前看到它、按 Esc 打断；微信侧只能事后收到摘要。** 所以桥必须支持从微信发 `/stop`（或 `/esc`），映射到主循环里 `Act::Interrupt` 那条路径——`cancel.cancel(); ui.stopping = true;`。

注意：`/stop` 必须在入站处**先于 `ui.queued` 拦截**，否则它会排在正在跑的 turn 后面，等 turn 跑完才生效，等于没有。这是本设计里唯一一个不能走统一队列的入站消息。

---

## 5. 实施顺序（完成状态）

1. ✅ 验证二进制：`crates/wechat/examples/verify.rs`（`cargo run -p wechat --example verify`），扫码登录 → 长轮询 → 回显。第 3 节协议侧 1–9 的**静态核实**已回填；「真实账号扫码」是唯一未做的实跑，留给第 7 节。
2. ✅ `crates/wechat` crate 成形：`types.rs`（wire 类型）/ `client.rs`（Client + 端点 + 超时）/ `login.rs`（二维码登录状态机 + QR 渲染）。已加进根 `Cargo.toml` workspace deps。
3. ✅ 凭据持久化：`~/.pi/wechat.json`（`tools::state::dir()` + pretty JSON，tmp+rename）。**注意**：原设计说「复用 keys.rs」，但 keys.rs 实为键盘绑定表，无凭据机制——改用 pi root 下的状态文件，见第 3 节第 11 条。
4. ✅ `Cmd::Wechat(String)` / `Step::Wechat(WechatCmd)` + `BUILTIN` 表 + `parse` + `Repl::command` 分支；`cargo check` 报出的两处 surface match（line.rs / tui/mod.rs）已补齐。
5. ✅ `crates/cli/src/wechat.rs` 桥：入站走 `ui.queued`（turn 内）或直接提交（空闲）、`/stop`/`/esc` 在入站处特判、出站 `observe(&Event)` 按 4.5 的策略发送、`Done`/中断时 flush。
6. ✅ TUI 接线完成。`line.rs` 侧按原设计降级：`/wechat on|off` 打印「需要终端显示二维码」，`/wechat` 报 off（line.rs 没有屏幕画 QR，见 handoff 说明）。
---

## 6. 已否决的方案（不要重新提出）

- **权限白名单 / 微信侧降级 Approver**：账号所有者判定只有本人能给该 bot 发消息，明确不做。
- **`pi --wechat` 无头守护进程、每联系人一个 session**：是上面这套跑通后的自然延伸，`line.rs` 就是现成骨架，但引入多 session 生命周期与 workspace 归属问题，第一版不做。
- **webhook / 内网穿透**：协议是长轮询，不需要。
- **媒体收发**：第一版纯文本。

---

## 7. 验证方式（现状）

- ✅ 协议层工具已保留：`crates/wechat/examples/verify.rs`。它复用 `~/.pi/wechat.json`，可直接当作端到端验证工具。
- ✅ 单元测试已按惯例补上：`crates/wechat` 的 serde/解析/退避测试、`repl.rs` 的 `parse("/wechat …")` 测试；`cargo test --workspace` 全绿（222+ 例）。
- ⏳ **真实账号端到端（唯一未完成项）**：本机 `cargo run -p pi` → `/wechat on` → 终端出现二维码 → 手机扫码 → 手机发「跑一下 cargo check」→ 确认手机收到工具摘要与最终回复、本地终端同一 session 可见、手机发 `/stop` 能打断。此步需要真人和真微信账号，无法在仓库内自动完成。
- ✅ `cargo check` / `cargo test --workspace` / `cargo clippy -p wechat -p cli` 均通过。
---

## 8. 风险

- 这套 API 是从 npm 包逆向出来的，**不是腾讯发布的接口文档**。字段随版本变动——`channel_version` 就是包版本号（当前 2.4.8），说明有版本协商；升级 npm 包后应复查本实现的常量与字段是否仍一致。
- 腾讯条款保留限速、拦截、阻断的权利（2.8 节）。功能可能某天单方面失效，这不是 bug。
- 合法性上它确实是腾讯官方产品、有专项条款背书，与 WeChatPadPro 那类逆向 iPad 协议不是一回事，封号风险按文档说法是「正常使用无风险」——但这句话来自逆向分析文，不来自腾讯。
