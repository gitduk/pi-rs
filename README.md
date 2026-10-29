# pi

A terminal coding agent in Rust. One binary, `pi`.

The goal: I need a capability, so I have it.

It began as a rewrite of the core of
[oh-my-pi](https://github.com/can1357/oh-my-pi); the message model is copied
from [rig](https://github.com/0xPlaygrounds/rig).

## Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gitduk/pi-rs/releases/latest/download/pi-installer.sh | sh
```

Prebuilt for Linux x86_64 and aarch64. Anywhere else, `cargo build --release`
leaves the binary at `target/release/pi`.

A pi installed over the running one, a rebuild of the same version too, takes
over every open session by itself once nothing on screen is half done. The
resumed session keeps a restart confirmation at the end of its transcript.
Consecutive identical confirmations share one row with a repeat count. At
startup pi also asks GitHub once for the latest release; a newer one shows on
the bar as `vX.Y.Z available`.

## Configure

There is no built-in model list. `~/.pi/settings.toml` names one endpoint and
the models on it:

```toml
base_url = "https://api.anthropic.com"
format   = "anthropic"          # anthropic | openai | chat
api_key  = "$ANTHROPIC_API_KEY" # a leading `$` reads the environment

[models."claude-sonnet-5"]
context_window    = 200_000
max_output_tokens = 64_000
```

| `format`    | API              | pi appends          |
| ----------- | ---------------- | ------------------- |
| `anthropic` | Messages         | `/v1/messages`      |
| `openai`    | Responses        | `/responses`        |
| `chat`      | Chat Completions | `/chat/completions` |

A model on another endpoint says so in its own entry; the rest keep the file's:

```toml
[models."gpt-5"]
base_url = "https://api.openai.com/v1"
format   = "openai"
api_key  = "$OPENAI_API_KEY"   # never the file's key: that one is for its host
```

[`examples/pi.toml`](examples/pi.toml) lists every key. An unknown key is
refused at load.

A `.pi.toml` between the workspace and the repository root overrides the same
keys, every one of them. The keys that reach past the checkout — `mcp`,
`hooks`, `keep_days`, `base_url`, `api_key`, and a model's own
`base_url` or `api_key` — are named under the banner when pi starts, and so
are the checkout's own script tools. Read both in a checkout you did not write
before running pi there.

Nothing needs a restart. Within a second of a save to the settings, the lane
in front is rebuilt from them; a file that does not parse is named once and the
old config stays. A system prompt file, another prompt replaced under
`~/.pi/prompts/`, an `AGENTS.md` or memory is read again at the next session or
compaction instead: changed mid-session, the prompt would contradict what the
model already read, and its cached prefix would be lost. `/edit` opens a prompt
not yet replaced on the built-in's text; saved unchanged, the file is removed,
so the built-in goes on following pi's updates.

The bar under the input line is laid out by `~/.pi/bar.rs`, a cargo script
(`cargo +nightly -Zscript`) that reads pi's state as JSON on stdin — `model`,
`effort`, `tier`, `running`, `root`, `worktree`, `ctx`, `spent`, `alerts`,
`update`, `released` — and prints a layout:
[`examples/bar.rs`](examples/bar.rs) is one to start from. Without the file,
the bar is `tabs`, `model` and `spent` on the left, `update` and `flash` on
the right. pi reruns it whenever its state changes, and every `refresh`
seconds if set. A layout is `lines`, each with `left` and `right` items and
an optional `sep` (a muted ` · ` by default). An item is a part's name;
`{"part", "style"}` to colour one; `{"text", "style"}`, the script's own; or
`{"group": [...]}`, items with nothing between them, gone whole when none of
its parts has anything to say. A style is a theme name (`muted`, `heading`,
`emphasis`, `code`, `input`, `ok`, `err`, `add`, `del`) or a style as
`[theme]` writes one. A part with nothing to say drops out with the separator
before it.

| Part        | Shows                                                     |
| ----------- | --------------------------------------------------------- |
| `tabs`      | the checkouts, folded to the room the row leaves          |
| `flash`     | the answer to the last key, for a moment                  |
| `model`     | the model                                                 |
| `effort`    | the thinking effort; absent while off                     |
| `tier`      | the tool tier                                             |
| `update`    | a release newer than this pi, or one installed under it   |
| `spent`     | what the session has cost so far                          |
| `cost`      | what the last run cost                                    |
| `elapsed`   | how long the last run took                                |
| `in_out`    | its tokens in and out                                     |
| `speed`     | its output tokens per second                              |
| `cache`     | how much of its input was cached                          |
| `ctx`       | the context used, against the window                      |
| `compacted` | how often the session was compacted                       |
| `queued`    | lines waiting for the run to end                          |
| `worktree`  | the worktree in front                                     |

From `cost` down, the parts are the status line's, read off the run in flight
or the one that last ended.

The default colours follow Codex's Catppuccin palettes: Mocha on dark terminal
backgrounds and Latte on light ones. Code blocks use syntax highlighting;
inline code, links and clickable hints such as `n more` and `more below` use
the theme blue (`#89b4fa`). Prose keeps the terminal foreground; headings use
a colour per level without `#` markers, bold text is
red, italic keeps its foreground, and secondary text and quotes use the
terminal's default foreground with `Dim`. Tool names, the input
sigil and selected items use the accent blue (`#63a8f8`). Shell commands use
syntax highlighting, and timing stays grey. If the terminal
does not report its background, pi uses Mocha. Existing `[theme]` styles still
override the defaults. For example:

```toml
[theme]
syntax = "auto" # or "catppuccin-mocha" / "catppuccin-latte"
link = { color = "#89b4fa", sgr = ["underline"] }
muted = { color = "39", sgr = ["dim"] } # 39: terminal's default foreground
quote = { color = "39", sgr = ["dim"] }
strong = { color = "#f38ba8", sgr = ["bold"] }
```

Markdown lists retain their nesting and hanging indent when wrapped. Tasks
show checkboxes, quotes have a rule at each level, and thematic breaks fit the
available width. Definition lists indent their descriptions. Links retain
their destinations and titles; bare URLs and email addresses are recognized
outside code. Images show their description, complete address and title.

Run statistics keep labels and separators grey, with blue token counts,
speed and cache figures, and a dim green cost. Context usage turns yellow
at 80% of the window and red at 95%. Archived statistics use the same colours;
explicitly styled bar parts keep their configured style.
While a run is active, its statistics start with a `⠴ ⠦ ⠖ ⠲` spinner;
once it ends, a grey `π ` keeps the figures in the same columns, including
when the archived line is redrawn.

Assistant prose and tool calls are separated by one blank line, with
continuous tool calls kept together. Separators scroll with their preceding
line and have no click action; tool calls keep the terminal background.
Reasoning uses the terminal's default foreground with `Dim` in the transcript;
its reading page uses normal Markdown contrast for comfortable reading of
long blocks.

## Run

```bash
pi                        # interactive
pi "fix the flaky test"   # one-shot: answer on stdout, nothing saved
echo "..." | pi           # prompt on stdin
pi -c                     # continue the last session in this directory
pi -C ~/repo --tier read  # another directory, read-only
```

`--tier` caps what a run may reach:

| Tier    | Reaches                                       |
| ------- | --------------------------------------------- |
| `read`  | any file on the machine                       |
| `write` | `read`, plus writing inside the workspace     |
| `exec`  | everything, through a shell. **The default.** |
| `net`   | `read`, plus the web                          |

**`bash` is not sandboxed.** It runs `sh -c` anywhere, and pi never asks
before a call. Under `exec` nothing is fenced, so `write` and `edit` reach
anywhere too rather than refuse what `bash` would do. Under `write` they are
held inside the workspace and pi's home, symlinks resolved. The home is
writable so the model can make its own tools and skills, which means it can
also edit `settings.toml`, and an edit there takes effect within a second. Use
`--tier` when that is too much.

A run has no turn cap. It ends when the model stops calling tools or you press
Esc. A session is open in one pi at a time: resuming one another pi holds is
refused, and resuming one from another directory moves it here.

## In a session

| Input                              | Does                                          |
| ---------------------------------- | --------------------------------------------- |
| `/new`, `/resume [id]`, `/name`    | start, switch, label a session                |
| `/rewind <said>`, `esc esc`        | go back to something you said                 |
| `/model [name]`                    | list the models, or move the session to one   |
| `/effort [level]`                  | `off` `low` `medium` `high`, from now on      |
| `/worktree [name]`, `/worktree rm` | work in `<repo>.worktrees/<name>`             |
| `/mv <dir>`                        | move this session to another directory; its jobs follow |
| `/compact [focus]`                 | summarize everything but the working tail     |
| `/loop <line>`                     | resubmit a line until a round edits no file   |
| `/<tool> [key=value…]`            | run a tool yourself beside any turn, as a background job; the model is told what it said |
| `/mcp`, `/mcp restart [name]`      | MCP servers and their tools; reconnect        |
| `/settings`                        | edit the project's config                     |
| `/edit [file]`                     | edit a prompt, an `AGENTS.md`, or memory      |
| `/status`, `/keys`, `/help`        | session paths and spend; bindings; commands   |
| `/exit`                            | leave; `ctrl+d` does the same                 |
| `/content`                         | everything the model is given before you type |
| `/<skill> [args]`                  | run a skill (also one-shot: `pi "/commit"`)   |
| `! <command>`                      | run a shell command and record its output     |
| `@path`                            | complete a workspace file                     |
| click a code block's label         | copy it; a diagram or table, its source       |
| drag, double or triple click       | copy the text, a word, or a line              |
| `ctrl+v`                           | paste a clipboard image as `[Image #n]`       |

Slash completion lists tools directly (for example, `/wechat`), then offers
their `key=value` arguments. Descriptions mark tools with `[tool]` and skills
with `[skill]`. Existing commands keep their names when a tool has the same
name; there is no `/tool` entry point.

In Vim Normal mode, `ctrl+k` focuses the last background job above the input.
`ctrl+k`/`ctrl+j` move between jobs; `ctrl+j` below the last job or `esc` returns
to the draft at its original cursor position. The selected job is highlighted:
`x` stops it, `r` restarts it, `X` stops every running job in this workspace,
and `enter` expands or folds its output. Stopped rows stay available while
focused so they can be restarted. Completion lists and reading pages keep
their own keys; Insert mode keeps its editing keys. `/keys` includes this help.

While a model reply is generating, completed background results and additional
messages you type wait for that reply to finish. After its tool calls complete,
they enter the next model request in the same run. A reply with no tool calls
also continues when messages are waiting. Generation is never interrupted.

## Tools

`read` `write` `edit` `glob` `grep` `bash` `fetch` `skill` `subagent` `memory`, plus:

- One tool per tool of each MCP server in `[mcp.<name>]`, offered as
  `<name>__<tool>` once the server has listed them. `command` and `args` run
  one over stdio, `url` reaches one over Streamable HTTP. A server that will
  not start is listed as `<name>__unavailable`, with the reason; `/mcp`
  shows each server and its tools, and `/mcp restart` reconnects them. A call
  with no answer in 10 minutes is given up.
- In interactive mode, `bash` runs commands and manages background work. A regular
  command waits at most 10 seconds, then returns a job id and the latest output while the original
  command keeps running. `background: true` returns immediately. Add `wait` to
  delay the start **in seconds**, for example:
  `{"command":"python3 report.py","background":true,"wait":30}`. The delay
  happens in the background; omit `wait` or use 0 to start immediately.
  With no command, `{}` lists jobs, `{"output":3}` shows a job's latest output,
  `{"stop":[3]}` stops it, `{"stop_all":true}` stops all, and `{"restart":[3]}`
  runs a command again under its id. Stopping during a delay prevents execution.
  Background results automatically open later turns once the checkout is idle;
  the model can continue independent work or report progress and end its turn.
  The separate job tool and its former completion-wait interface are removed.
  One-shot runs such as `pi "fix the flaky test"` wait for each command to exit,
  without automatically moving it to the background or imposing a timeout.
  Interrupting the run stops the command and its process group.
  Subagents keep their commands isolated and stop them when their owning run
  ends; their shell offers `join` to collect results before ending.
  One-shot runs and subagents do not offer delayed background starts.
  A `subagent` call with `background`, or a script that detaches, uses the same
  job table and shell controls. Jobs appear above the input line; clicking one
  shows its output. The task-list keys and bash controls stop or restart jobs;
  user actions are reported to the model before it next speaks. Jobs last while
  pi runs; past that, use cron.
- A script is executable code. A tool is a named interface the model can call
  directly. A tool can use a script as its implementation.
  Keep task scripts in the workspace. Run these scripts through `bash`.
  Running a script repeatedly does not require a new tool.
  Reserve the home and project `tools/` directories for registered tools.
  New project tools belong in the project's `.pi/tools/` by default.
  This includes tools that depend on project code, data, paths, or configuration.
  The model receives the resolved project tool path in `<project_tools>`.
  Use the global tool directory only when the user requests a tool shared
  across projects. A write boundary that blocks the project directory does
  not make the global directory a fallback.
  Keep a skill's helper scripts inside that skill's directory.
  One tool per script in `~/.pi/tools/`: any file whose first line is a `#!`,
  its `description` and `[args]` written as TOML in a `# ---` comment block
  under that line; or a cargo script `*.rs`, described by its
  `[package] description` and `[package.metadata.pi.args]`, which needs
  `cargo +nightly -Zscript`. A script that does not read is listed as not
  usable, with the reason. `~/.pi/tools/.env` holds `NAME=value` lines every
  script starts with, such as its service's key; `bash` never sees them.
  A checkout's own scripts go in the nearest `.pi/tools/` up to the
  repository root (outside a repository, the workspace's own), with its own
  `.env`; of a name both use, the checkout's is the one offered, and the
  other is reported as skipped. A built-in tool's name is not free either:
  a script that takes one keeps it, and the built-in stands down with a note
  saying to rename one of them.
  A script may also write JSON lines to fd 3 (`PI_EVENTS_FD`): `status` for
  its progress, `detach` to end the call and run on as a job, then `result`
  for each turn it sends back (its stdout at exit is the last), `input` for a
  person's line, sent to the model as written, never run as a command
  (what only the person reads, a QR say, goes to stderr: they open the job
  to see it), `choose` with
  `options` to ask a question as the `choose` tool does (stopping the job
  takes it back), `interrupt` to stop the checkout's turn, and `follow` to
  hear every turn there. A turn an input opened, or any once it follows, is
  told back on fd 3: `started`, optional `{"progress": "..."}` messages,
  then `reply` with the remaining answer (excluding text already sent as
  progress). Progress includes completed model text before tool calls and
  changed tool or job output, paced while work continues. One that follows is
  also told each question up on the checkout, as
  `{"question": {"id", "text", "options", "back"}}`, then
  `{"closed": <id>}` once it is answered anywhere or withdrawn; it answers
  one with `{"answer": <id>, "picks": [0]}`, by place, the pick going out as
  if typed there, or hears `{"refused": <id>, "why"}` when it is not taken,
  and an `input` closes every question that does not wait.
  `choose` works before a detach too, where pi answers it with one line on
  fd 3: `{"asked": true}` once it is up, or an `error` saying why not. There
  `"answer": "back"` keeps the pick from going out: it comes down fd 3 as
  `{"answer": "<option>"}`, your own words if you wrote some, or
  `{"answer": null}` when you closed it — for a script that waits on you,
  such as one that asks before letting a command run.

  A question has up to 9 options, each the line you would send to pick it —
  words for the model, or a command such as `/compact`. They are the list
  over the input line, shown even when it is empty, as a completion list is
  and with its keys: `↑`/`↓` (or `ctrl+j`/`ctrl+k`, and `j`/`k` in vim's
  Normal) move, `enter` sends the highlighted one, `tab` takes it onto the
  line to send as it is or changed, and `esc` (`q` in Normal) closes the
  question unanswered. Typing narrows the options by their words or number;
  words that match none are your own answer, sent with `enter`. To pick
  several, `space` marks them while nothing is typed (or in Normal), and
  `enter` sends the marked ones as one prompt, a line each; a command is
  only picked alone.
  [`examples/tools/`](examples/tools/) has five to copy: `search`; `judge`,
  which reads its key from there; `choose`, through which the model puts
  you a question and ends its turn; `wechat`, which bridges the session
  to a WeChat chat — ask for it, scan the QR the first time, and what you
  send from the phone arrives as turns, `/stop` included, and a question
  arrives numbered, answered by replying with its number, or several such
  as `1 3`; and `telegram`, the same over a Telegram bot, a question's
  options as buttons, a tap answering with one: put a
  token from @BotFather in `.env` as
  `TELEGRAM_BOT_TOKEN`, open the link or QR it shows the first time, and
  only the account that opened it is heard (delete `~/.pi/telegram.json` to
  pair another). Both are cargo scripts, `wechat.rs` and `telegram.rs`, so
  they need nightly cargo and build on first use. Both forward progress
  before the run ends and retry temporary send failures. Unsent pieces stay
  in memory, in order, until another incoming message triggers a retry;
  stopping the channel discards that queue. Updating them requires copying
  the scripts again and restarting their jobs along with the updated pi.

A server's prompts are commands too: `/<name>:<prompt> [args]` asks the server
for the prompt's text and sends it, one typed word per argument, the last
taking the rest.

`[[hooks]]` run a command on events, handed the event as one line of JSON on
stdin. Around a tool call they are gates: on `tool.before`, exit 0 lets the
call run and exit 2 refuses it with what the hook printed, and a hook that
fails or hangs refuses it too; on `tool.after`, what it printed joins the
result. Around a run they are waited on too: `run.start` comes before its
first request, with the prompt, and what it prints on exit 0 is a note to the
model; `run.stop` comes when the model would stop, with its reply, and exit 2
keeps the run going with what the hook printed — at most three times a run.
The other events — `tool.denied`, `turn.start`, `turn.end` (with the reply's
text), `compacted`, `retrying`, `warning`, `run.done`, `run.failed` (with the
error; a stop you asked for is not one) — are only watched: queued off the run, one at a time, nothing they print reaching it. A
subagent's calls pass the same gates, but its events stay its own.
A hook has fd 3 as a script does, so it can `choose` with `"answer": "back"`
and wait on you, as long as you take: `examples/hooks/ask-first` holds a
`bash` call that cannot be taken back until you allow it.
`examples/pi.toml` has the shape.

Scripts and skills are read live: one written while pi runs is offered to the
model from its next turn, and a skill's `/name` answers the next time you type.
A skill is a directory with a `SKILL.md` in `~/.pi/skills/`, or in the nearest
`.pi/skills/` up to the repository root (outside a repository, the workspace's
own) — the same two places a script is read from. Where both hold one name,
for a script as for a skill, the project's is the one offered.

The built-in `pi-extend` skill teaches the model to write both and to connect
an MCP server, so a capability it lacks is one it can add; the built-in
`pi-help` skill is this README, for questions about pi itself. A skill of
either name on disk replaces it.

An image pasted with `ctrl+v` goes with the message itself; its path rides
along in the text. `read` also opens PNG, JPEG, GIF and WebP files as images.
A model reaches them only with `vision = true` in its `[models]` entry; without
it, each image is sent as a line saying it was left out.

`fetch` speaks http and https and refuses loopback, private and link-local
addresses. With [rtk](https://github.com/rtk-ai/rtk) on `PATH`, `bash` runs
each command through `rtk rewrite`; `RTK_DISABLED=1` turns that off.

What a command prints twice reaches the model once. A run of six or more lines
repeating an earlier one byte for byte, like the same diff printed for every
failing test, becomes one line quoting where the first copy begins.

When the transcript outgrows the window, pi drops repeated and old tool
results first and summarizes whole rounds last, down to half the room it
has, so the next compaction is many turns away. The saved session keeps
everything; only the model's view shrinks.

pi remembers without being asked. When you correct it, say what you prefer,
or settle a decision, the model writes a line to `~/.pi/memory/` with its
`memory` tool, and you see each line go in as it does; the next session or
compaction carries it. Memory holds 16 KiB in all: a change that would grow it
past that is refused until the model folds or removes lines. The files are
plain markdown, named by `/status`; edit or delete them freely. A subagent
never writes memory, nor does a run under `--tier read` or `net`, or with
`--no-context-files`.

## Files

`PI_HOME` moves `~/.pi`.

| Path                                     | Holds                                      |
| ---------------------------------------- | ------------------------------------------ |
| `~/.pi/settings.toml`                    | endpoint, models, `[keys]`, `[theme]`      |
| `~/.pi/SYSTEM.md`                        | replaces the built-in system prompt        |
| `~/.pi/prompts/summarize.md`             | replaces the compaction summary prompt     |
| `~/.pi/prompts/subagent.md`              | replaces a subagent's own prompt           |
| `~/.pi/AGENTS.md`, `AGENTS.md`           | instructions: yours, a project's           |
| `~/.pi/skills/`                          | skills: a directory with a `SKILL.md`      |
| `.pi/skills/`                            | a project's skills                         |
| `~/.pi/memory/*.md`                      | memory: global, and one file per project   |
| `~/.pi/tools/`                           | script tools                               |
| `~/.pi/tools/.env`                       | values every script starts with; `0600`    |
| `.pi/tools/`                             | a project's script tools                   |
| `~/.pi/images/`                          | images pasted with `ctrl+v`                |
| `~/.pi/bar.rs`                           | a cargo script that lays out the bar       |
| `~/.pi/sessions/<project>/<session>/`    | the transcript and `journal.jsonl`         |

A session not worked on for 90 days, and a pasted image not pasted again for
as long, is deleted when pi starts; `keep_days` in `settings.toml` changes the
days, and `0` keeps everything.

The journal records what the transcript does not: requests, timings, refusals,
retries. One JSON object per line, `0600`, kept two weeks. `PI_LOG` takes
`debug`, `trace` or `off`.

## Develop

`cargo test` and `cargo clippy --all-targets`, both expected clean. Run
`cargo fmt` before committing. Pushing a `vX.Y.Z` tag builds and publishes the
release on CI.

## Not built

LSP, a sandbox for `bash`, per-call approval, session branching.
