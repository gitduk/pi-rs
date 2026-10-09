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
`base_url` or `api_key` — are named under the banner when pi starts. Read the
file in a checkout you did not write before running pi there.

Nothing needs a restart. Within a second of a save to the settings, a system
prompt file, an `AGENTS.md` or memory, the lane in front is rebuilt from them;
a file that does not parse is named once and the old config stays.

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
`settings.toml`, and an edit there takes effect within a second. `bash` runs `sh -c` with only its working
directory held there, and pi never asks before a call. Use `--tier` when that
is too much.

A run has no turn cap. It ends when the model stops calling tools or you press
Esc.

## In a session

| Input                              | Does                                          |
| ---------------------------------- | --------------------------------------------- |
| `/new`, `/resume [id]`, `/name`    | start, switch, label a session                |
| `/model [name]`                    | list the models, or move the session to one   |
| `/effort [level]`                  | `off` `low` `medium` `high`, from now on      |
| `/worktree [name]`, `/worktree rm` | work in `<repo>.worktrees/<name>`             |
| `/compact [focus]`                 | summarize everything but the working tail     |
| `/loop <line>`                     | resubmit a line until a round edits no file   |
| `/later`, `/later rm <id>`         | what the model left for later; cancel one     |
| `/mcp`, `/mcp restart [name]`      | MCP servers and their tools; reconnect        |
| `/settings`                        | edit the project's config                     |
| `/status`, `/keys`, `/help`        | session paths and spend; bindings; commands   |
| `/exit`                            | leave; `ctrl+d` does the same                 |
| `/content`                         | everything the model is given before you type |
| `/wechat on`, `/wechat off`        | bridge the session to a WeChat chat           |
| `/<skill> [args]`                  | run a skill (also one-shot: `pi "/commit"`)   |
| `! <command>`                      | run a shell command and record its output     |
| `@path`                            | complete a workspace file                     |
| click a code block's label         | copy it; a diagram or table, its source       |
| drag, double or triple click       | copy the text, a word, or a line              |
| `ctrl+v`                           | paste a clipboard image as `[Image #n]`       |

## Tools

`read` `write` `edit` `glob` `grep` `bash` `fetch` `skill` `subagent`, plus:

- `judge`, when the config has a `[judge]` section.
- One tool per tool of each MCP server in `[mcp.<name>]`, offered as
  `<name>__<tool>` once the server has listed them. `command` and `args` run
  one over stdio, `url` reaches one over Streamable HTTP. A server that will
  not start is listed as `<name>__unavailable`, with the reason; `/mcp`
  shows each server and its tools, and `/mcp restart` reconnects them.
- `later`, in the terminal: a prompt the model leaves itself, which comes back
  as a turn of its own after a delay, on a period, or when a background
  command exits. It waits until the checkout is in front and idle, and lasts
  while pi runs; past that, schedule `pi "..."` with the system's cron.
- One tool per script in `~/.pi/tools/`: any file whose first line is a `#!`,
  its `description` and `[args]` written as TOML in a `# ---` comment block
  under that line; or a cargo script `*.rs`, described by its
  `[package] description` and `[package.metadata.pi.args]`, which needs
  `cargo +nightly -Zscript`. A script that does not read is listed as not
  usable, with the reason.

A server's prompts are commands too: `/<name>:<prompt> [args]` asks the server
for the prompt's text and sends it, one typed word per argument, the last
taking the rest.

`[[hooks]]` run a command around tool calls, handed
the call as JSON on stdin. Before a call, exit 0 lets it run and exit 2 refuses
it with what the hook printed; a hook that fails or hangs refuses it too. After
one, what it printed joins the result. `examples/pi.toml` has
the shape.

Scripts and skills are read live: one written while pi runs is offered to the
model from its next turn, and a skill's `/name` answers the next time you type.

The built-in `pi-extend` skill teaches the model to write both and to connect
an MCP server, so a capability it lacks is one it can add; the built-in `pi-help`
skill is this README, for questions about pi itself. A skill of either name in
`~/.pi/skills/` replaces it.

An image pasted with `ctrl+v` goes with the message itself; its path rides
along in the text. `read` also opens PNG, JPEG, GIF and WebP files as images. A model reaches them
only with `vision = true` in its `[models]` entry; without it, each image is
sent as a line saying it was left out.

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

pi remembers without being asked. When pi exits, a process it leaves behind
reads what sessions said since it last looked and updates `~/.pi/memory/`,
with `summarize_model` when one is set; the next start carries the result. The
files are plain markdown, named by `/status`; edit or delete them freely.
Memory starts with the first session to end and never reads tool output.

## Files

`PI_HOME` moves `~/.pi`.

| Path                                     | Holds                                      |
| ---------------------------------------- | ------------------------------------------ |
| `~/.pi/settings.toml`                    | endpoint, models, `[keys]`, `[theme]`      |
| `~/.pi/SYSTEM.md`                        | replaces the built-in system prompt        |
| `~/.pi/AGENTS.md`, `AGENTS.md`           | standing instructions: yours, a project's  |
| `~/.pi/skills/`                          | skills: a directory with a `SKILL.md`      |
| `~/.pi/memory/*.md`                      | memory: global, and one file per project   |
| `~/.pi/tools/`                           | script tools                               |
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
