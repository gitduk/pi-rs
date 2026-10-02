# pi

A terminal coding agent in Rust. One binary, `pi`.

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

[`examples/pi.toml`](examples/pi.toml) lists every key. An unknown key is
refused at load.

A `.pi.toml` between the workspace and the repository root overrides the same
keys, `base_url`, `api_key`, `system` and `write_roots` included. Read the one
in a checkout you did not write before running pi there.

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

**`bash` is not sandboxed.** `write` and `edit` are held inside the workspace
and `write_roots`, symlinks resolved. `bash` runs `sh -c` with only its working
directory held there, and pi never asks before a call. Use `--tier` when that
is too much.

A run has no turn cap. It ends when the model stops calling tools or you press
Esc.

## In a session

| Input                              | Does                                          |
| ---------------------------------- | --------------------------------------------- |
| `/new`, `/resume [id]`, `/name`    | start, switch, label a session                |
| `/model [name]`                    | list the models, or move the session to one   |
| `/worktree [name]`, `/worktree rm` | work in `<repo>.worktrees/<name>`             |
| `/compact [focus]`                 | summarize everything but the working tail     |
| `/loop <line>`                     | resubmit a line until a round edits no file   |
| `/reload`, `/settings`             | re-read the config, or edit the project's     |
| `/status`, `/keys`, `/help`        | session paths and spend; bindings; commands   |
| `/wechat on`, `/wechat off`        | bridge the session to a WeChat chat           |
| `/<skill> [args]`                  | run a skill (also one-shot: `pi "/commit"`)   |
| `! <command>`                      | run a shell command and record its output     |
| `@path`                            | complete a workspace file                     |

## Tools

`read` `write` `edit` `glob` `grep` `bash` `fetch` `skill` `subagent`, plus:

- `judge`, when the config has a `[judge]` section.
- One tool per cargo script in `~/.pi/tools/*.rs`. Its `[package] description`
  is the tool's description and `[package.metadata.pi.args]` its arguments.
  Needs `cargo +nightly -Zscript`.

`fetch` speaks http and https and refuses loopback, private and link-local
addresses. With [rtk](https://github.com/rtk-ai/rtk) on `PATH`, `bash` runs
each command through `rtk rewrite`; `RTK_DISABLED=1` turns that off.

When the transcript outgrows the window, pi drops repeated and old tool
results first and summarizes whole rounds last. The saved session keeps
everything; only the model's view shrinks.

## Files

`PI_HOME` moves `~/.pi`.

| Path                                     | Holds                                      |
| ---------------------------------------- | ------------------------------------------ |
| `~/.pi/settings.toml`                    | endpoint, models, `[keys]`, `[theme]`      |
| `~/.pi/AGENTS.md`, `AGENTS.md`           | standing instructions: yours, a project's  |
| `~/.agents/skills/`, `.agents/skills/`   | skills: a directory with a `SKILL.md`      |
| `~/.pi/tools/*.rs`                       | script tools                               |
| `~/.pi/bar.rs`                           | a cargo script that lays out the bar       |
| `~/.pi/sessions/<project>/<session>/`    | the transcript and `journal.jsonl`         |

The journal records what the transcript does not: requests, timings, refusals,
retries. One JSON object per line, `0600`, kept two weeks. `PI_LOG` takes
`debug`, `trace` or `off`.

## Develop

`cargo test` and `cargo clippy --all-targets`, both expected clean. Run
`cargo fmt` before committing. Pushing a `vX.Y.Z` tag builds and publishes the
release on CI.

## Not built

MCP, LSP, a sandbox for `bash`, per-call approval, session branching.
