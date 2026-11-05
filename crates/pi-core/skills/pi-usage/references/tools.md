# Tools

The built-in tools are `read`, `write`, `edit`, `glob`, `grep`, `bash`, `fetch`,
`skill`, `subagent`, and `memory`.
MCP servers and scripts can add tools.

## MCP tools and prompts

Each server in `[mcp.<name>]` supplies tools as `<name>__<tool>` after it lists
them. `command` and `args` start a server over stdio. `url` connects over
Streamable HTTP. If a server cannot start, pi lists `<name>__unavailable` with
the reason. `/mcp` shows servers and tools. `/mcp restart` reconnects them.

pi stops waiting for an unanswered call after 10 minutes.
A server's prompts are also commands.
`/<name>:<prompt> [args]` requests the prompt text from the server and submits
it. Each argument receives one typed word. The last argument receives the
remaining text.

## Shell commands

`bash` waits for a foreground command to exit.
In interactive mode, `background: true` returns a command ID immediately.
For example: `{"command":"python3 report.py","background":true}`.

The same tool controls background commands:

- `{}` lists background commands.
- `{"output":3}` reads a command's output.
- `{"output":3,"wait":true}` waits until the command prints something new, sends
  a result, or ends, then reads only what is new.
- `{"stop":[3]}` stops commands.
- `{"restart":[3]}` starts commands again under their existing IDs.

`output`, `stop`, and `restart` each accept one ID, a list of IDs, or `"all"`.
They reject empty lists so an unintended empty value cannot affect every command.
Background results open later turns when the checkout is idle.
Commands appear above the input. Click a command to open its output.

Shell calls show the original command with shell highlighting.
Named wrappers show the tool name and supplied arguments with tool-call colours.
Expanding a command shows the actual launch command, working directory, and
output. The original call and launch specification survive restart and session
recovery. User stop and restart actions reach the model before its next
response.

pi stops a foreground command after ten minutes.
It never automatically moves that command to the background.
Interrupting the run stops the command's process group.
One-shot runs and subagents offer only foreground commands.
A subagent call also waits for completion.
Background commands stop when pi exits.

Use cron for work that must continue after pi exits.

## Script tools

A script is executable code.
A tool is a named interface the model can call directly.
A tool can use a script as its implementation.
Keep task scripts in the workspace. Run these scripts through `bash`.
Repeated script execution does not require a new tool.

Reserve the home and project `tools/` directories for registered tools.
New project tools belong in the project's `.pi/tools/` by default.
This includes tools that depend on project code, data, paths, or configuration.
The model receives the resolved path in `<project_tools>`.
Use the global directory only when the user requests a tool shared across
projects. If the write boundary blocks the project directory, do not use the
global directory instead.

Keep a skill's helper scripts inside that skill's directory.
Each supported script in `~/.pi/tools/` defines one tool.
Use one of these formats:

- A script whose first line is `#!`. Write `description`, `[required]`, and
  `[optional]` as TOML in a `# ---` comment block immediately below that line.
- A cargo script named `*.rs`. Use `[package] description`,
  `[package.metadata.pi.required]`, and `[package.metadata.pi.optional]`.
  This format requires `cargo +nightly -Zscript`.

You can omit either argument table.
Each entry maps a parameter name to its string description.
If pi cannot read a script's interface, it lists the tool as unusable with the
reason.

`~/.pi/tools/.env` holds `NAME=value` lines for the scripts' initial
environment, such as a service key.
A background wrapper loads these values only for its subprocess.
Ordinary `bash` calls do not receive them.
For checkout scripts, use the nearest `.pi/tools/` up to the repository root.
Outside a repository, use the workspace's `.pi/tools/`.
This directory has its own `.env`.

When both directories contain the same name, pi offers the checkout's tool.
It reports the other tool as skipped.
Built-in tools also keep their names.
pi skips a script that uses a built-in name and reports the conflict.

To create a named background wrapper, set `background = true` in the tool's
metadata. For a shebang tool, use the top level of its comment block.
For a Rust tool, use `[package.metadata.pi]`.
The wrapper runs through the bash background executor.
Manual calls such as `/telegram` keep their names.

The wrapper supplies JSON on stdin.
It copies declared arguments into the environment.
It saves the launch specification for restart.
These wrappers are available only in interactive sessions.

## Script events

A script can write JSON lines to file descriptor 3 (fd 3), specified by
`PI_EVENTS_FD`.
What it may say there, and what pi writes back, is `references/events.md`.

## Questions

A question has up to 9 options.
Each option is the line the person would send to select it, so it speaks in
their voice and calls the model `you`.
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
To select several options, use `space` while the input is empty or in Normal
mode. `enter` sends the marked options as one prompt, one option per line.
A command must be selected alone.

## Example tools

The repository's `examples/tools/` holds tools to copy:

- `search` searches the web.
- `judge` asks a judgment model and reads its key from the tool directory's
  `.env`.
- `choose` asks the person a question.
  The model then ends its turn.
- `wechat` connects the session to a WeChat chat.
  Request it, then scan its QR code on first use.
  Phone messages arrive as turns, including `/stop`.
  Questions show numbered options.
  Reply with one number or several, such as `1 3`.
- `telegram` connects through a Telegram bot and shows question options as
  buttons. Tap a button to select one option.
  Put a token from @BotFather in `.env` as `TELEGRAM_BOT_TOKEN`.
  Open the link or scan the QR code on first use.
  pi accepts messages only from the account that opened it.
  Delete `~/.pi/telegram.json` to pair another account.

`wechat.rs` and `telegram.rs` require nightly cargo and compile on first use.
Both send progress before the run ends and retry temporary send failures.
Unsent pieces stay in memory, in order, until another incoming message triggers
a retry. Stopping the channel discards that queue.
To update either tool, copy the script again.
Restart its background command along with the updated pi.

## Hooks

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
These are `tool.denied`, `turn.start`, `turn.end`, `compacted`, `retrying`,
`warning`, `run.done`, and `run.failed`.
`turn.end` includes the reply text. `run.failed` includes the error.
A user stop is not a `run.failed` event.
pi queues these hooks separately from the run, one at a time.

Their output does not reach the model.
A subagent's calls pass the same gates, but its events stay with the subagent.
A hook has fd 3, as a script does.
It can use `choose` with `"answer": "back"` and wait for the person without a
time limit.
The repository's `examples/hooks/ask-first` holds an irreversible `bash` call
until the person permits it.
The `pi-setup` skill's own `settings.toml` shows the configuration format.

## Skills

pi reads script and skill directories during the session.
A new script or skill becomes available to the model on its next turn.
A skill's `/name` command becomes available the next time you type.
A skill is a directory containing `SKILL.md`.
Use `~/.pi/skills/` or the nearest `.pi/skills/` up to the repository root.
Outside a repository, use the workspace's `.pi/skills/`.

These follow the same location rules as scripts.
If both locations contain the same script or skill name, pi offers the project's
version.

The built-in `tool-creator` skill teaches the model to write tools.
`skill-creator` teaches it to write skills.
`pi-setup` explains how to add a model provider or an MCP server.
`pi-usage` holds these pages: what a session, a tool, a script or a file does.
`architecture` guides the model to review a project's structure, or to design
structure before it builds a change.
`bar-setup` lays out the bar.
A skill with the same name on disk replaces the built-in.
