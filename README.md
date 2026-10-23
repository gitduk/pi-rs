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
over every open session by itself once nothing on screen is half done. At
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
`hooks`, `keep_days`, `write_roots`, `base_url`, `api_key`, and a model's own
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

**`bash` is not sandboxed.** `write` and `edit` are held inside the workspace,
`write_roots` and pi's home, symlinks resolved. The home is writable so the
model can make its own tools and skills, which means it can also edit
`settings.toml`, and an edit there takes effect within a second. `bash` runs
`sh -c` with only its working directory held there, and pi never asks before a
call. Use `--tier` when that is too much.

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
| `/cd <dir>`                        | carry this conversation on in another directory; its jobs follow |
| `/compact [focus]`                 | summarize everything but the working tail     |
| `/loop <line>`                     | resubmit a line until a round edits no file   |
| `/job [text]`, `stop <id>`         | background jobs; have the model start one; stop one |
| `/tool [name] [key=value…]`        | list the tools, or run one yourself beside any turn, as a job `/job stop` ends; the model is told what it said |
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

## Tools

`read` `write` `edit` `glob` `grep` `bash` `fetch` `skill` `subagent` `memory`, plus:

- One tool per tool of each MCP server in `[mcp.<name>]`, offered as
  `<name>__<tool>` once the server has listed them. `command` and `args` run
  one over stdio, `url` reaches one over Streamable HTTP. A server that will
  not start is listed as `<name>__unavailable`, with the reason; `/mcp`
  shows each server and its tools, and `/mcp restart` reconnects them. A call
  with no answer in 10 minutes is given up.
- `job`, in the terminal: what runs in the background, the latest a
  command printed with `output`, or `stop` one. A
  `bash` or `subagent` call with `background`, or a script that detaches,
  leaves the turn free; each result comes back as a turn of its own once the
  checkout is in front and idle. Those still out are listed above the input line,
  a click on one shows what it printed, and `/job stop` stops one — the
  model is told before it next speaks. `/job <text>` asks the model to start
  something in the background; `/tool bash command=… background=true` starts
  one without it. Jobs last while pi runs; past that, use cron.
- One tool per script in `~/.pi/tools/`: any file whose first line is a `#!`,
  its `description` and `[args]` written as TOML in a `# ---` comment block
  under that line; or a cargo script `*.rs`, described by its
  `[package] description` and `[package.metadata.pi.args]`, which needs
  `cargo +nightly -Zscript`. A script that does not read is listed as not
  usable, with the reason. `~/.pi/tools/.env` holds `NAME=value` lines every
  script starts with, such as its service's key; `bash` never sees them.
  A checkout's own scripts go in the nearest `.pi/tools/` up to the
  repository root (outside a repository, the workspace's own), with its own
  `.env`; yours keeps a name both use.
  A script may also write JSON lines to fd 3 (`PI_EVENTS_FD`): `status` for
  its progress, `detach` to end the call and run on as a job, then `result`
  for each turn it sends back (its stdout at exit is the last), `input` for a
  person's line, sent to the model as written, never run as a command,
  `notice` for a line on the screen the model never reads, `choose` with
  `options` to ask a question as the `choose` tool does (stopping the job
  takes it back), and `interrupt` to stop the checkout's turn. A turn an
  input opened is told back on fd 3: `started`, then its whole `reply`.
  `choose` works before a detach too, where pi answers it with one line on
  fd 3: `{"asked": true}` once it is up, or an `error` saying why not. There
  `"answer": "back"` keeps the pick from going out: it comes down fd 3 as
  `{"answer": "<option>"}`, your own words if you wrote some, or
  `{"answer": null}` when you closed it — for a script that waits on you,
  such as one that asks before letting a command run.

  A question has up to 9 options, each the line you would send to pick it —
  words for the model, or a command such as `/compact`. It opens just over
  the input and holds the keyboard: `j`/`k` move, `enter` or a digit sends
  one, `q` or `esc` closes it, and any other key does nothing. `i` hands you
  the line to write your own answer, which closes the question once sent;
  emptying the line returns you to the options. With `multiple`, `space` or
  a digit marks several, and `enter` sends the marked ones as one prompt, a
  line each; those options are words, never commands.
  [`examples/tools/`](examples/tools/) has five to copy: `search`; `judge`,
  which reads its key from there; `later`, a prompt the model leaves itself
  for after a delay or on a period; `choose`, through which the model puts
  you a question and ends its turn; and `wechat`, which bridges the session
  to a WeChat chat — ask for it, scan the QR the first time, and what you
  send from the phone arrives as turns, `/stop` included. It is a cargo
  script, `wechat.rs`, so it needs nightly cargo and builds on first use.

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

The built-in `pi-extend` skill teaches the model to write both and to connect
an MCP server, so a capability it lacks is one it can add; the built-in
`pi-help` skill is this README, for questions about pi itself. A skill of
either name in `~/.pi/skills/` replaces it.

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
