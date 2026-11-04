# pi

A terminal coding agent in Rust. One binary, `pi`.

The goal: I need a capability, so I have it.

It began as a rewrite of the core of
[oh-my-pi](https://github.com/can1357/oh-my-pi).
Its message model comes from [rig](https://github.com/0xPlaygrounds/rig).

## Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gitduk/pi-rs/releases/latest/download/pi-installer.sh | sh
```

Prebuilt for Linux x86_64 and aarch64. Anywhere else, `cargo build --release`
leaves the binary at `target/release/pi`.

A new pi installation automatically takes over every open session when no screen operation remains incomplete.
This also applies to a rebuild of the same version.
Each resumed session keeps a restart confirmation at the end of its transcript.
Consecutive identical confirmations share one row with a repeat count. At
startup pi also asks GitHub once for the latest release.
The bar shows a newer release as `vX.Y.Z available`.

## Configure

Configure a provider in `~/.pi/settings.toml`; pi discovers its models:

```toml
[provider.local]
base_url = ":3425/v1"          # http://127.0.0.1:3425/v1
format = "chat"               # anthropic | openai | chat
# api_key = "$PROVIDER_API_KEY"
```

Connection settings belong in `[provider.<name>]`. `format` selects the API
protocol for that connection:

| `format`    | API              | pi appends          |
| ----------- | ---------------- | ------------------- |
| `anthropic` | Messages         | `/v1/messages`      |
| `openai`    | Responses        | `/responses`        |
| `chat`      | Chat Completions | `/chat/completions` |

pi reads `<base_url>/models` (`<base_url>/v1/models` for `anthropic`).
`/model` lists the discovered models and offers them for completion.
Select one with `/model local/model-id` or start with `pi -m local/model-id`.
The provider prefix distinguishes identical IDs on different hosts; only the
original model ID is sent to the server. A sole provider also accepts bare IDs
that contain no `/`. With no model selected by a flag, resumed session or the
top-level `model` setting, pi starts on the first discovered name in alphabetical order.

The default response mapping reads the `data` array and each item's `id`,
`context_window` and `max_output_tokens`. Capability and price facts are read
only where a path is given. Override only the paths that differ:

```toml
[provider.go]
base_url = "https://opencode.ai/zen/go/v1"
format = "chat"
api_key = "$OPENCODE_API_KEY"
models_path = "data"           # default; could be "models" or "result.models"
id = "id"                     # default; relative to each item
context_window = "limit.context"
max_output_tokens = "limit.output"
vision = "modalities.input"              # a bool, or input types where any `image/*` counts
accepts_temperature = "capabilities.temperature"
reasoning = "capabilities.reasoning.supported"   # false means no effort levels

[provider.go.pricing]
input_per_mtok = "cost.text.input.price"
output_per_mtok = "cost.text.output.price"
```

Rates are per million tokens unless the pricing table says `unit = "token"`,
which rescales a catalog that quotes a per-token figure.

`api_key = "$NAME"` reads an environment variable; another string is a literal
key. Set `api_key` explicitly to use a credential, including one from the
environment. Anthropic's `cache_control` setting also belongs in its provider table.

`[provider.<name>.headers]` sends a header with every request that connection
makes, catalog reads included:

```toml
[provider.local.headers]
x-opencode-session = "$OPENCODE_SESSION"   # an unset variable leaves it out
```

A header name is a token, and a value is printable ASCII. The wire sets its own
`authorization`, `x-api-key` and `anthropic-version`, so a header of the same
name is sent twice rather than replacing them.

Paths use dot-separated object fields. An empty `models_path` selects an array
at the response root; an empty `id` selects a string item itself.
`models_url` can specify a separate, complete catalog URL.
A row without an ID is skipped, and a fact the catalog states unreadably leaves
that fact unset; a path that selects no ID at all is an error.
Missing token limits use the existing defaults: a 128,000-token window and
8,192 output tokens. `[models."provider/model-id"]` contains limit, capability
and price overrides:

```toml
[models."local/model-id"]
context_window = 200_000
vision = true
```

Explicit fields override discovered values. Discovery never writes the settings
file. Catalogs refresh in the background every five minutes and when `/model`
is used without a name. Each discovery attempt has a five-second timeout;
failed refreshes keep the last successful list. Responses with `has_more` and
`last_id` are paginated using `after_id`.

[`examples/pi.toml`](examples/pi.toml) lists every key. An unknown key is
refused at load.

A `.pi/settings.toml` between the workspace and repository root can override any
of these keys. Files merge before required provider fields are checked, so a
project can change only a field mapping. If a project changes a provider's
`base_url` or `models_url`, it must also set `api_key` and `headers` for that
target: neither the original connection's key nor its headers are inherited.
At startup, pi lists keys that affect resources outside the checkout under the banner.
These include `mcp`, `hooks`, `keep_days`, and each provider's `base_url`,
`models_url`, `api_key` and `headers`.
pi also lists the checkout's script tools.
Inspect both lists before you run pi in a checkout you did not write.

You do not need to restart pi after a settings change.
Within a second of a save, pi rebuilds the active lane from the settings.
If the file does not parse, pi reports it once and keeps the old configuration.
Other lanes apply refreshed model facts and their own project settings when
they become active or start a turn.

pi reads system prompts, replacement prompts in `~/.pi/prompts/`, `AGENTS.md`, and memory again at the next session or compaction.
A prompt change during a session would contradict text the model already read.
It would also invalidate the cached prefix.
For a prompt without a replacement, `/edit` opens the built-in text.
If you save it unchanged, pi removes the replacement file.
The prompt then continues to receive pi's built-in updates.

The default main and subagent prompts share an execution policy.
A replacement system or subagent prompt replaces both the role and that policy.
pi still appends the run's workspace, permissions, standing instructions, and memory when enabled.
`/content` shows the current system and tool definitions.
It also shows each system block's source, size, and diagnostic fingerprint.
Fingerprints identify content changes, not authenticity.

To customize the bar, use the built-in `bar-creator` skill.
It includes the `bar.rs` example, input and layout formats, styles, and refresh behavior.

The default colours use the cool colours from Codex's Catppuccin palettes:
Mocha on dark terminal backgrounds and Latte on light ones.
If the terminal does not report its background, pi uses Mocha.
Existing `[theme]` styles still override these defaults.

Code blocks keep syntax highlighting.
Inline code, links, and clickable hints such as `n more` and `more below` use blue.
Prose keeps the terminal foreground.
Headings use Codex's level styles without `#` markers:

- H1 uses red, bold, underlined text.
- H2 uses peach, bold text.
- H3 uses yellow, bold, italic text.
- H4–H6 use yellow, italic text.

Bold text uses theme green.
Italic text and quotes use teal.
Wide tables shown as source alternate column text between blue and green.
The current search match uses blue with reversed colours.
Secondary text uses the terminal's default foreground with `Dim`.

Tool names and command sigils use Codex's accent blue.
This is `#63a8f8` on dark backgrounds and `#1c64c8` on light ones.
Selected menu rows use the input background with bold theme-blue text.
Success and failure use the terminal's green and red.
Warnings use yellow on dark backgrounds and the terminal foreground on light ones.

Input and submitted text keep their original background shading, derived from the terminal background.
The input shading is more pronounced.
Both areas start at the left edge without a prefix or padding.
Shell commands use syntax highlighting.
Timing stays grey.

For example:

```toml
[theme]
syntax = "auto" # or "catppuccin-mocha" / "catppuccin-latte"
link = { color = "#89b4fa", sgr = ["underline"] }
muted = { color = "39", sgr = ["dim"] } # 39: terminal's default foreground
quote = { color = "39", sgr = ["dim"] }
strong = { color = "#a6e3a1", sgr = ["bold"] }
```

Markdown lists retain their nesting and hanging indent when text wraps.
Tasks show checkboxes.
Quotes have a rule at each level.
Thematic breaks fit the available width.
Definition lists indent their descriptions.

Links retain their destinations and titles.
pi recognizes bare URLs and email addresses outside code.
Images show their description, complete address, and title.

Run statistics keep labels and separators grey, with blue token counts,
speed and cache figures, and a dim green cost. Context usage takes the warning
style at 80% of the window and the error style at 95%.
Archived statistics use the same colours.
Explicitly styled bar parts keep their configured style.
While a run is active, its statistics start with a breathing `π` marker.
After the run ends, a grey `π ` keeps the figures in the same columns, including when pi redraws the archived line.

pi separates assistant prose and tool calls with one blank line.
Consecutive tool calls stay together.
Separators scroll with their preceding line and have no click action.
Tool calls keep the terminal background.

Reasoning uses the terminal's default foreground with `Dim` in the transcript.
Its reading page uses normal Markdown contrast for long blocks.

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

**`bash` is not sandboxed.**
It runs `bash -c` anywhere, and pi never asks before a call.
Under `exec`, `write` and `edit` can also change files anywhere.
They do not refuse changes that `bash` could make.
Under `write`, they can change files only inside the workspace and pi's home, after resolving symlinks.

The home is writable so the model can create tools and skills.
This also lets it edit `settings.toml`.
An edit there takes effect within a second.
Use `--tier` when you need stricter limits.

A run has no turn limit.
It ends when the model stops calling tools or you press Esc.
Only one pi can open a session at a time.
pi refuses to resume a session that another pi holds.
Resuming a session from another directory moves it here.

## In a session

| Input                              | Does                                          |
| ---------------------------------- | --------------------------------------------- |
| `/new`, `/resume [id]`, `/name`    | start, switch, label a session                |
| `/rewind <said>`, `esc esc`        | go back to something you said                 |
| `/model [name]`                    | list the models, or move the session to one   |
| `/effort [level]`                  | `off` `low` `medium` `high`, from now on      |
| `/worktree [name]`, `/worktree rm` | work in `<repo>.worktrees/<name>`             |
| `/mv <dir>`                        | move this session and its background commands to another directory |
| `/compact [focus]`                 | summarize everything but the working tail     |
| `/loop <line>`                     | resubmit a line until a round edits no file   |
| `/<tool> [key=value…]`            | run a named tool. Background tools return immediately |
| `/mcp`, `/mcp restart [name]`      | list MCP servers and tools, or reconnect      |
| `/settings`                        | edit the project's config                     |
| `/edit [file]`                     | edit a prompt, an `AGENTS.md`, or memory      |
| `/status`, `/keys`, `/help`        | show session paths and spend, bindings, or commands |
| `/exit`                            | exit pi, as with `ctrl+d`                     |
| `/content`                         | show current system blocks, their sources, and tool definitions |
| `/<skill> [args]`                  | run a skill (also one-shot: `pi "/commit"`)   |
| `!<command>`                       | run a shell command and record its output     |
| `!!<command>`                      | start one in the background and keep typing   |
| `@path`                            | complete a workspace file                     |
| click a code block's label         | copy the code, or the source of a diagram or table |
| drag, double or triple click       | copy the text, a word, or a line              |
| `ctrl+v`                           | paste a clipboard image as `[Image #n]`       |

Slash completion lists tools directly (for example, `/wechat`), then offers
their `key=value` arguments. Descriptions mark tools with `[tool]` and skills
with `[skill]`. Existing commands keep their names when a tool has the same
name.
There is no `/tool` entry point.

In Vim Normal mode, `ctrl+k` focuses the last background command above the input.
`ctrl+k` and `ctrl+j` move between commands.
`ctrl+j` below the last command or `esc` returns to the draft at its original cursor position.
pi highlights the selected command.

- `x` stops the selected command.
- `r` restarts the selected command.
- `X` stops every running command in this workspace.
- `enter` expands or folds the selected command's output.

Stopped rows stay available while focused so you can restart them.
Completion lists and reading pages keep their own keys.
Insert mode keeps its editing keys.
`/keys` includes this help.

While a model reply is generating, completed background results and additional
messages you type wait for that reply to finish. After its tool calls complete,
they enter the next model request in the same run. A reply with no tool calls
also continues when messages are waiting. Generation is never interrupted.
Waiting messages show under the status line, and `up` takes them back. Typed
messages use a separate queue, so commands waiting for the run to end do not
hold them back. Remote inputs continue to wait for their own run.

A `!` command waits for the run to release the transcript.
It then runs to completion without a timeout.
A `!!` command uses the same background executor as the model's background commands.
It returns an ID immediately, without waiting for the transcript or command completion.
You can start it while a reply is generating.
Its output reaches the model when the command ends, together with other waiting results.

## Tools

The built-in tools are `read`, `write`, `edit`, `glob`, `grep`, `bash`, `fetch`, `skill`, `subagent`, and `memory`.
MCP servers and scripts can add tools.

### MCP tools and prompts

Each server in `[mcp.<name>]` supplies tools as `<name>__<tool>` after it lists them.
`command` and `args` start a server over stdio.
`url` connects over Streamable HTTP.
If a server cannot start, pi lists `<name>__unavailable` with the reason.
`/mcp` shows servers and tools.
`/mcp restart` reconnects them.

pi stops waiting for an unanswered call after 10 minutes.
A server's prompts are also commands.
`/<name>:<prompt> [args]` requests the prompt text from the server and submits it.
Each argument receives one typed word.
The last argument receives the remaining text.

### Shell commands

`bash` waits for a foreground command to exit.
In interactive mode, `background: true` returns a command ID immediately.
For example: `{"command":"python3 report.py","background":true}`.

The same tool controls background commands:

- `{}` lists background commands.
- `{"output":3}` reads a command's output.
- `{"output":3,"wait":true}` waits until the command prints something new, sends a result, or ends, then reads only what is new.
- `{"stop":[3]}` stops commands.
- `{"restart":[3]}` starts commands again under their existing IDs.

`output`, `stop`, and `restart` each accept one ID, a list of IDs, or `"all"`.
They reject empty lists so an unintended empty value cannot affect every command.
Background results open later turns when the checkout is idle.
Commands appear above the input.
Click a command to open its output.

Shell calls show the original command with shell highlighting.
Named wrappers show the tool name and supplied arguments with tool-call colours.
Expanding a command shows the actual launch command, working directory, and output.
The original call and launch specification survive restart and session recovery.
User stop and restart actions reach the model before its next response.

pi stops a foreground command after ten minutes.
It never automatically moves that command to the background.
Interrupting the run stops the command's process group.
One-shot runs and subagents offer only foreground commands.
A subagent call also waits for completion.
Background commands stop when pi exits.

Use cron for work that must continue after pi exits.

### Script tools

A script is executable code.
A tool is a named interface the model can call directly.
A tool can use a script as its implementation.
Keep task scripts in the workspace.
Run these scripts through `bash`.
Repeated script execution does not require a new tool.

Reserve the home and project `tools/` directories for registered tools.
New project tools belong in the project's `.pi/tools/` by default.
This includes tools that depend on project code, data, paths, or configuration.
The model receives the resolved path in `<project_tools>`.
Use the global directory only when the user requests a tool shared across projects.
If the write boundary blocks the project directory, do not use the global directory instead.

Keep a skill's helper scripts inside that skill's directory.
Each supported script in `~/.pi/tools/` defines one tool.
Use one of these formats:

- A script whose first line is `#!`.
  Write `description`, `[required]`, and `[optional]` as TOML in a `# ---` comment block immediately below that line.
- A cargo script named `*.rs`.
  Use `[package] description`, `[package.metadata.pi.required]`, and `[package.metadata.pi.optional]`.
  This format requires `cargo +nightly -Zscript`.

You can omit either argument table.
Each entry maps a parameter name to its string description.
If pi cannot read a script's interface, it lists the tool as unusable with the reason.

`~/.pi/tools/.env` holds `NAME=value` lines for the scripts' initial environment, such as a service key.
A background wrapper loads these values only for its subprocess.
Ordinary `bash` calls do not receive them.
For checkout scripts, use the nearest `.pi/tools/` up to the repository root.
Outside a repository, use the workspace's `.pi/tools/`.
This directory has its own `.env`.

When both directories contain the same name, pi offers the checkout's tool.
It reports the other tool as skipped.
Built-in tools also keep their names.
pi skips a script that uses a built-in name and reports the conflict.

To create a named background wrapper, set `background = true` in the tool's metadata.
For a shebang tool, use the top level of its comment block.
For a Rust tool, use `[package.metadata.pi]`.
The wrapper runs through the bash background executor.
Manual calls such as `/telegram` keep their names.

The wrapper supplies JSON on stdin.
It copies declared arguments into the environment.
It saves the launch specification for restart.
These wrappers are available only in interactive sessions.

### Script events

A script can write JSON lines to file descriptor 3 (fd 3), specified by `PI_EVENTS_FD`.

| Event | Purpose |
| --- | --- |
| `status` | Report progress. |
| `result` | Send a result that opens a turn. Stdout at exit supplies the final result. |
| `input` | Submit a person's line as if typed, including commands. A leading space sends slash-prefixed text as prose. |
| `choose` with `options` | Ask a question, as with the `choose` tool. Stopping the command withdraws it. |
| `interrupt` | Stop the checkout's turn. |
| `follow` | Receive every turn in the checkout. |

Send content only the person needs, such as a QR code, to stderr.
The person opens the command to read it.
pi reports each turn opened by an input through fd 3.
After `follow`, it also reports other turns in the checkout.
The sequence is `started`, optional `{"progress": "..."}` messages, then `reply`.
`reply` contains the remaining answer, excluding text already sent as progress.

Progress includes completed model text before tool calls and changed tool or command output.
pi paces these updates while work continues.
A following script also receives each question in the checkout:

1. `{"question": {"id", "text", "options", "back"}}` when the question appears.
2. `{"closed": <id>}` when someone answers or withdraws it.

Answer a question with `{"answer": <id>, "picks": [0]}`, using option positions.
pi submits the selection as if the person typed it.
If pi rejects the answer, the script receives `{"refused": <id>, "why"}`.
An `input` closes every question that does not wait.

Foreground scripts can use `choose`.
pi answers on fd 3 with `{"asked": true}` when the question appears, or an `error` that explains the failure.
With `"answer": "back"`, pi returns the selection to the script instead of submitting it:

- `{"answer": "<option>"}` for a selected option.
- The person's own words if they wrote an answer.
- `{"answer": null}` when the person closes the question.

Use this mode for a script that waits for the person, such as approval before a command runs.

### Questions

A question has up to 9 options.
Each option is the line the person would send to select it.
It can contain words for the model or a command such as `/compact`.
The option list appears above the input, even when the input is empty.
It uses completion-list keys:

- `↑` and `↓` move between options.
  Alternatives are `ctrl+j` and `ctrl+k`, or `j` and `k` in Vim Normal mode.
- `enter` sends the highlighted option.
- `tab` copies the option to the input for submission or editing.
- `esc`, or `q` in Normal mode, closes the question unanswered.

Typing narrows the options by their words or number.
If no option matches, `enter` sends the typed words as the person's answer.
To select several options, use `space` while the input is empty or in Normal mode.
`enter` sends the marked options as one prompt, one option per line.
A command must be selected alone.

### Example tools

[`examples/tools/`](examples/tools/) contains five tools to copy:

- `search` searches the web.
- `judge` asks a judgment model and reads its key from the tool directory's `.env`.
- `choose` asks the person a question.
  The model then ends its turn.
- `wechat` connects the session to a WeChat chat.
  Request it, then scan its QR code on first use.
  Phone messages arrive as turns, including `/stop`.
  Questions show numbered options.
  Reply with one number or several, such as `1 3`.
- `telegram` connects through a Telegram bot and shows question options as buttons.
  Tap a button to select one option.
  Put a token from @BotFather in `.env` as `TELEGRAM_BOT_TOKEN`.
  Open the link or scan the QR code on first use.
  pi accepts messages only from the account that opened it.
  Delete `~/.pi/telegram.json` to pair another account.

`wechat.rs` and `telegram.rs` require nightly cargo and compile on first use.
Both send progress before the run ends and retry temporary send failures.
Unsent pieces stay in memory, in order, until another incoming message triggers a retry.
Stopping the channel discards that queue.
To update either tool, copy the script again.
Restart its background command along with the updated pi.

### Hooks

Each `[[hooks]]` entry runs a command for specified events.
It receives the event as one JSON line on stdin.
Tool hooks control or supplement calls:

- On `tool.before`, exit 0 permits the call.
  Exit 2 refuses it with the hook's output.
  A failure or timeout also refuses the call.
- On `tool.after`, the hook's output joins the result.

pi also waits for run hooks:

- `run.start` runs before the first model request and receives the prompt.
  Its output on exit 0 becomes a note to the model.
- `run.stop` runs when the model would stop and receives its reply.
  Exit 2 continues the run with the hook's output.
  This can happen at most three times per run.

Other events only notify hooks.
These are `tool.denied`, `turn.start`, `turn.end`, `compacted`, `retrying`, `warning`, `run.done`, and `run.failed`.
`turn.end` includes the reply text.
`run.failed` includes the error.
A user stop is not a `run.failed` event.
pi queues these hooks separately from the run, one at a time.

Their output does not reach the model.
A subagent's calls pass the same gates, but its events stay with the subagent.
A hook has fd 3, as a script does.
It can use `choose` with `"answer": "back"` and wait for the person without a time limit.
`examples/hooks/ask-first` holds an irreversible `bash` call until the person permits it.
See `examples/pi.toml` for the configuration format.

### Skills

pi reads script and skill directories during the session.
A new script or skill becomes available to the model on its next turn.
A skill's `/name` command becomes available the next time you type.
A skill is a directory containing `SKILL.md`.
Use `~/.pi/skills/` or the nearest `.pi/skills/` up to the repository root.
Outside a repository, use the workspace's `.pi/skills/`.

These follow the same location rules as scripts.
If both locations contain the same script or skill name, pi offers the project's version.

The built-in `tool-creator` skill teaches the model to write tools.
`skill-creator` teaches it to write skills.
`mcp-setup` explains how to connect MCP servers.
`architecture` guides the model to review a project's structure, or to design structure before it builds a change.
`pi-help` loads an index for questions about pi.
The model reads relevant README sections through the skill's `file` argument.
It can request `README.md` when it needs the complete documentation.
A skill with the same name on disk replaces the built-in.

### Images, web access, and context

An image pasted with `ctrl+v` accompanies the message.
The message text includes its path.
`read` also opens PNG, JPEG, GIF, and WebP files as images.
The model receives images only with `vision = true` in its `[models."provider/model-id"]` entry.
Otherwise, pi replaces each image with a line that explains the omission.

`fetch` supports HTTP and HTTPS.
It refuses loopback, private, and link-local addresses.
With [rtk](https://github.com/rtk-ai/rtk) on `PATH`, `bash` passes each command through `rtk rewrite`.
`RTK_DISABLED=1` disables this step.

Repeated command output reaches the model once.
pi replaces six or more byte-identical repeated lines with one line that identifies the first copy.
For example, this applies to the same diff printed for each failing test.

When the transcript exceeds the context window, pi removes repeated and old tool results first.
It summarizes whole rounds last, reducing the context to half the available room.
This leaves room for many turns before the next compaction.
The saved session keeps everything.
Only the model's view shrinks.
Long user inputs keep their beginning and end in the history sent to the summarizer.
The summary records skills that still apply and requirements specific to the task.
The model may need to load a skill again when its instructions no longer remain in context.

### Memory

pi records memory without a request.
When you correct it, state a preference, or settle a decision, the model writes a line with `memory`.
The line goes to `~/.pi/memory/`, and pi shows it as the model writes it.
The next session or compaction includes that memory.

Memory has a total limit of 16 KiB.
pi refuses a change that would increase memory beyond this limit until the model combines or removes lines.
The files are plain Markdown.
`/status` lists their paths.
You can edit or delete them.
Subagents and runs with `--tier read`, `--tier net`, or `--no-context-files` never write memory.

## Files

`PI_HOME` moves `~/.pi`.

| Path                                     | Holds                                      |
| ---------------------------------------- | ------------------------------------------ |
| `~/.pi/settings.toml`                    | providers, models, `[keys]`, `[theme]`     |
| `~/.pi/SYSTEM.md`                        | replaces the built-in system prompt        |
| `~/.pi/prompts/summarize.md`             | replaces the compaction summary prompt     |
| `~/.pi/prompts/subagent.md`              | replaces a subagent's own prompt           |
| `~/.pi/AGENTS.md`, `AGENTS.md`           | instructions: yours, a project's           |
| `~/.pi/skills/`                          | skills: a directory with a `SKILL.md`      |
| `.pi/skills/`                            | a project's skills                         |
| `~/.pi/memory/*.md`                      | memory: global, and one file per project   |
| `~/.pi/tools/`                           | script tools                               |
| `~/.pi/tools/.env`                       | initial environment values for scripts, mode `0600` |
| `.pi/tools/`                             | a project's script tools                   |
| `~/.pi/images/`                          | images pasted with `ctrl+v`                |
| `~/.pi/bar.rs`                           | a cargo script that lays out the bar       |
| `~/.pi/sessions/<project>/<session>/`    | the transcript and `journal.jsonl`         |

At startup, pi deletes sessions with no activity for 90 days.
It also deletes pasted images with no repeated paste for 90 days.
`keep_days` in `settings.toml` changes this interval.
`0` keeps everything.

The journal records what the transcript does not: requests, timings, refusals,
retries. One JSON object per line, `0600`, kept two weeks. `PI_LOG` takes
`debug`, `trace` or `off`.

## Develop

Run `cargo test`.
Run `cargo clippy --all-targets`.
Both must pass without warnings or failures.
Run `cargo fmt` before you commit.
Pushing a `vX.Y.Z` tag builds and publishes the release on CI.

## Not built

LSP, a sandbox for `bash`, per-call approval, session branching.
