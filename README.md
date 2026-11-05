# pi

A terminal coding agent in Rust. One binary, `pi`.

The goal: I need a capability, so I have it.

It began as a rewrite of the core of
[oh-my-pi](https://github.com/can1357/oh-my-pi).
Its message model comes from [rig](https://github.com/0xPlaygrounds/rig).

What it does not do is as deliberate as what it does. There is no built-in list
of models: it would go stale the week a vendor ships something. Connections are
configured once, and pi discovers the models each one serves. Tools and skills
are directories, read while pi runs, so a new one is available on the next turn
without a restart. And pi configures itself: it carries the instructions for
setting up a connection, so asking it to add a provider is the same act as
adding one.

## Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gitduk/pi-rs/releases/latest/download/pi-installer.sh | sh
```

Prebuilt for Linux x86_64 and aarch64. Anywhere else, `cargo build --release`
leaves the binary at `target/release/pi`.

## Run it

1. Copy the shipped settings file, then fill in one key:

   ```bash
   mkdir -p ~/.pi && cp examples/settings.toml ~/.pi/settings.toml
   ```

   The first section is live and the rest is commented out as the field
   reference. Set `api_key` — the example reads `$ANTHROPIC_API_KEY` from the
   environment — or replace that provider with the one you have.

2. Run it:

   ```bash
   pi "where is the retry logic"
   ```

`pi` alone starts a session; `pi -c` continues the last one in this directory;
`/help` lists every command. `pi --help` lists the flags.

With no model selected, pi starts on the first discovered name in alphabetical
order. Name one as `provider/model-id`: `pi -m main/claude-sonnet-5`. Every
model the configured connections serve appears in `/model`.

## What it may reach

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
This also lets it edit `~/.pi/settings.toml`.
An edit there takes effect within a second.

A run has no turn limit.
It ends when the model stops calling tools or you press Esc.
Only one pi can open a session at a time.

## Configure

`~/.pi/settings.toml` holds every connection: providers, models, MCP servers,
key bindings, and colours. [`examples/settings.toml`](examples/settings.toml)
is the field reference — every key, what it does, and the defaults.

To add or repair a provider or an MCP server, ask pi: its built-in `pi-setup`
skill reads that same file, so the model writes the shape you will later read.
That skill lives in
[`crates/pi-core/skills/pi-setup/`](crates/pi-core/skills/pi-setup/).

A `.pi/settings.toml` between the workspace and repository root can override any
key for that checkout alone. Files merge before required provider fields are
checked, so a project can change only a field mapping. If a project changes a
provider's `base_url` or `models_url`, it must also set `api_key`, `headers` and
`session_header` for that target: nothing written for the original connection is
inherited.

At startup, pi lists keys that affect resources outside the checkout under the
banner. These include `mcp`, `hooks`, `keep_days`, and each provider's
`base_url`, `models_url`, `api_key`, `headers` and `session_header`.
pi also lists the checkout's script tools.
Inspect both lists before you run pi in a checkout you did not write.

You do not need to restart pi after a settings change.
If the file does not parse, pi reports it once and keeps the old configuration.

`PI_HOME` moves `~/.pi`. Sessions, tools, skills, memory and pasted images all
live under it; `~/.pi/bar.rs` is a cargo script that lays out the bar, and
`/status` prints the session's paths and spend.

## Develop

Run `cargo test`.
Run `cargo clippy --all-targets`.
Both must pass without warnings or failures.
Run `cargo fmt` before you commit.
Pushing a `vX.Y.Z` tag builds and publishes the release on CI.

## Not built

LSP, a sandbox for `bash`, per-call approval, session branching.
