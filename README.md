# pi

A coding agent that stays inside one directory.

Rust, six crates, no framework. It reads and edits files, runs commands,
searches, keeps a plan, and compacts its own transcript when the window fills.
One binary, `pi`.

It started as an exercise in extracting the core of
[oh-my-pi](https://github.com/can1357/oh-my-pi) and rewriting it, and the
message model borrows its shape from [rig](https://github.com/0xPlaygrounds/rig)
— copied rather than depended on, so an upstream release cannot move it.

## Build

```bash
cargo build --release          # target/release/pi
```

Nothing to install and nothing to configure at build time. There is no
`PATH` entry created for you; symlink it where you want it.

## Configure

There is **no built-in list of models**. A hand-written catalog goes stale the
week a vendor ships something, and nothing in it tells you which entries still
describe reality — so the models a run can reach are the ones written down in
`~/.pi.toml`, against the endpoint they are actually pointed at.

```toml
[models.flash]
base_url = "http://127.0.0.1:7896/v1"
wire = "openai"                 # openai | anthropic
context_window = 1_000_000
max_output_tokens = 384_000
api_key = "x"
```

That is a complete config. With exactly one model defined you do not name a
default — there is nothing else it could mean — so `pi` runs it.

[`examples/pi.toml`](examples/pi.toml) is the full reference: every field, every
compat key with the default its wire uses, and measured starting points for a
few real endpoints. It is documentation, not a table that claims to be current.

**Two wires.** `anthropic` is the Messages API, `openai` is Chat Completions.
Each carries a `compat` record of quirks — whether the host accepts sampling
parameters, which field it wants the token cap in, whether it streams usage —
and a config names only the ones that differ from the wire's defaults. A key
that is not one of them is refused at load, with the list of the ones that are:
a typo there otherwise leaves a quirk at its default and produces a 400 much
later, pointing at nothing.

**Projects may configure, not redirect.** A `.pi.toml` inside a repository may
set `model`, `effort`, `max_turns` and `max_tier`, and nothing else. It arrives
by `git clone`; a base url, a key or a system-prompt path would let a checkout
point the run at a server of its own or name any file on disk to be sent to the
provider. `max_tier` applies downward only, so a repository can declare itself
read-only but cannot hand itself the shell — not even past an explicit
`--tier`.

## Run

```bash
pi                             # interactive
pi "fix the flaky test"        # one-shot
echo "..." | pi                # prompt on stdin
pi -c                          # continue the last session here
pi -C ~/some-repo --tier read  # elsewhere, read-only
```

Interactive is the default at a terminal. Everything is printed to stderr
except the answer, which goes to stdout so it pipes.

### The terminal

One loop owns the terminal for the whole session. Finished output is pushed
*above* the live region and becomes ordinary scrollback — selectable,
searchable, still there after exit. Only what is still changing is repainted.

Keys are rebindable. `/keys` lists every action with what it is bound to; an
id under `[keys]` in `~/.pi.toml` replaces that action's defaults:

```toml
[keys]
"app.clear-screen" = "ctrl+g"
"move.line.start"  = ["home", "f5"]
```

The namespace says what the action touches, and that is also what decides when
it is live — `edit.*` and `move.*` whenever you are typing, `menu.*` only with
the completion list open, `run.*` only during a turn. Two actions may share a
key when they are never live together, which is how `up` is `menu.previous`
with the list open and `history.older` without it. Sharing one *within* a
context is refused at load, along with an unknown id or an unreadable binding.

`/help` `/new` `/name` `/compact` `/reload` `/keys` `/todo` `/cost` `/exit`. Typing `/` opens a
list of what the line could still become; `↑` `↓` pick, `Tab` accepts, `Esc`
dismisses it until the next keystroke.

`/compact [what to keep in view]` summarizes everything outside the tail
you are working from — for when a phase has ended and no budget can tell.
`/name` and `--name` label a session, because the ids are timestamps.

`/reload` re-reads the config, the standing instructions and the skills. It
fails whole or not at all: a broken config leaves what was running running,
and says what was wrong with it. The model is not among what reloads —
reasoning blocks only replay to the model that produced them, which is the same
reason a resumed session stays on its own. There is no narrower `/reload keys`
because there is nothing to save: an unchanged system prompt is the same
string, so the provider's cache survives a reload that changed nothing.

### Standing instructions

`~/.pi.md` for yours, `AGENTS.md` for a project's — walked up to the repository
root, general first, so where two disagree the nearer directory is the one read
last. Appended to the system prompt, which is the part a provider caches.
`--no-context-files` turns it off.

### Skills

One name at both levels: `.agents/skills` in the project (walked up to the
repository root) and `~/.agents/skills`. That is the shared standard rather
than ours; carrying a private name beside it — anyone's, ours included — only
leaves the question of where a skill belongs permanently open. A directory
under some other name reaches the list by being symlinked into this one.

A skill is a directory with a `SKILL.md` carrying `name` and `description`.
Only descriptions are always in context; the body loads when the model asks for
it. Anything unreadable is reported at startup rather than vanishing.

### Structured output

```bash
pi --schema report.json "summarize the failures" | jq .
```

The run must end by calling `yield` with an object matching the schema, which
goes to stdout instead of prose.

## What it does

**Tools.** `read` `write` `edit` `glob` `grep` `bash` `todo` `skill` `yield`.
Each declares a tier — read, write or exec — and `--tier` caps the run. Every
path is resolved against the workspace root through the deepest existing
ancestor, so a symlink cannot walk out. `bash` gets its own process group and a
SIGTERM-then-SIGKILL timeout.

**Edits** are line-anchored patches with content-hash anchors, applied against
original line numbers so an earlier hunk never shifts a later one. A stale
anchor is refused rather than applied to the wrong place. Concurrent edits to
one file serialize per path — otherwise both pass their tag check and one
change disappears silently.

**Compaction** is a ladder: supersede a read that a later read replaced, elide
an uneventful result, age one out, and only then summarize before dropping. The
session log is append-only; compaction writes a *record* of what it dropped and
the model's view is derived from it, so the history that made the session worth
reading survives.

**Failures** are classified before they are retried. A spent quota and a
throttle both arrive as HTTP 429 and only the message text separates them —
retrying the first burns money. An overflow refusal usually names the real
window, so the correction is read out of it rather than guessed.

**Token counts** come from the provider where it reports them and from our own
count where it does not, and a `~` marks which half is which. A count we made
is not the provider's, and a cost derived from it is not a bill.

## Layout

| crate | | |
|---|---|---|
| `brain` | 2.7k | messages, wires, streams, faults, estimates |
| `agent` | 3.4k | the turn loop, compaction, the session log |
| `tools` | 3.6k | the tool set and the workspace gate |
| `cli` | 3.7k | terminal, config, sessions |
| `hashline` | 1.2k | the patch format — pure, no IO |
| `syntax` | 0.4k | tree-sitter outlines for eight languages |

~15k lines, 307 tests. `cargo test` runs everything; `cargo clippy
--all-targets` is expected to be silent.

## Not built

MCP, subagents, LSP, message-level cache breakpoints, session branching
(the log carries ids for it; nothing uses them yet), `Ctrl-Z` suspend.
