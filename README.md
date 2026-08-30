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
base_url = "http://127.0.0.1:7896/v1"
format   = "openai"             # openai | anthropic
api_key  = "x"

[models.flash]
context_window = 1_000_000
max_output_tokens = 384_000
```

That is a complete config. pi talks to one endpoint at a time, so the endpoint
is the file itself and a model is named the way that endpoint names it. With
exactly one model described you do not name a default — there is nothing else
it could mean — so `pi` runs it.

[`examples/pi.toml`](examples/pi.toml) is the full reference: every field with
the default it carries, and measured starting points for a few real endpoints.
It is documentation, not a table that claims to be current.

**Two wires.** `anthropic` is the Messages API, `openai` is the Responses API.
Those are the two native shapes and pi speaks nothing else: a server offering
only Chat Completions belongs behind a gateway that translates. A host's quirks
are ordinary fields on the provider or the model — whether it takes sampling
parameters, whether a forced tool choice sticks, whether it caches — each with
a stated default. A key that is not one of them is refused at load, with the
list of the ones that are: a typo there otherwise leaves a quirk at its default
and produces a 400 much later, pointing at nothing.

**Projects may configure, not redirect.** A `.pi.toml` inside a repository may
set `model`, `effort` and `max_tier`, and nothing else. It arrives
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

Colours are configurable the same way, under `[theme]` in `~/.pi.toml`. Every
key is a Style: a colour, text attributes, or both. `muted` `heading`
`emphasis` `code` `input`, plus `diff.add` `diff.del`, `status.ok` `status.err`,
`menu.selected` and `prompt.color` `prompt.icon`. A plain string is shorthand
for a colour alone — `code = "#dd80ff"` (or the short `#f80`, same as
`"38;2;221;128;255"`); a table takes the full form
`{ color = …, sgr = ["bold", "italic"] }`. Attributes are names (bold, dim,
italic, underline, blink, reverse, strike) or any SGR parameter list passed
through. `prompt.icon` is the one value that is neither colour nor attribute.
A key that is not one of those is refused at load, like a misspelled compat
key.

`/help` `/new` `/resume` `/name` `/model` `/compact` `/reload` `/keys` `/log` `/todo` `/cost` `/exit`, and one
more for every skill on disk. Typing `/` opens a list of what the line could
still become; `↑` `↓` pick, `Tab` accepts, `Esc` dismisses it until the next
keystroke. `/model` and `/resume` complete their arguments too: the model
name is the tedious part the config already knows, and a saved session is
named by its first question.

`/new` starts a fresh session, keeping this one on disk; `ctrl+l` twice does
the same (once clears the screen). `/resume` lists the sessions saved for this
workspace, newest first, by the first thing each was asked, and `/resume <id>`
switches to one — the session you leave is saved first, so nothing is lost on
the way out.

A line that starts with `!` is a shell command, not a prompt: `! git status`
runs it and shows the output, and the command and its result are recorded in
the transcript so the model answers with them in view. It runs in the same
workspace, under the same timeout, with the same clamps as the model's own
`bash` tool. `!` alone is just a prompt.
`/compact [what to keep in view]` summarizes everything outside the tail
you are working from — for when a phase has ended and no budget can tell.
`/name` and `--name` label a session, because the ids are timestamps.

`/model` on its own lists what `~/.pi.toml` defines — wire, window, price —
with a mark against the one running. `/model <name>` moves the session to
another, transcript and all. Prior reasoning does not survive the move intact:
a block carries the model that produced it, and a model that did not write it
gets the text without the signature, as prose or wrapped in `<think>` depending
on what it accepts. Nothing is rewritten on the way, so switching back makes
the original blocks native again. What has been spent stays spent — every turn
was priced by the model that ran it.

`/reload` re-reads the config, the standing instructions and the skills. It
fails whole or not at all: a broken config leaves what was running running,
and says what was wrong with it. The model is not among what reloads — which
model a session is on is a decision, not a preference, and `/model` is how it
changes. There is no narrower `/reload keys` because there is nothing to save:
an unchanged system prompt is the same string, so the provider's cache survives
a reload that changed nothing.

### The journal

Every session keeps one, at `~/.local/state/pi/logs/<session>.jsonl`, and
`/log` says where. A run opens the journal of the session it starts on —
`--resume` included — and `/resume` or `/new` switches it to the session now
in charge, so the whole of a session reads as one file across the runs that
touched it.
message, tool call and result, so the journal holds what never reaches a
message — which config was read, what went on the wire and what came back, how
long each turn and each tool took, why a patch was refused, what the loop
decided when it compacted or retried or gave up. A bug is read back from the
two together rather than reproduced.

One JSON object per line, so `jq` is the reader:

```bash
J=~/.local/state/pi/logs/<session>.jsonl              # /log prints the path
jq -c 'select(.lvl=="WARN" or .lvl=="ERROR")' $J      # only what went wrong
jq -c 'select(.ev=="pi::span")|{msg,name,dur_ms}' $J  # what took the time
jq -r 'select(.ev=="pi::edit")|.patch' $J             # what the model actually wrote
```

`ms` is milliseconds since the run began, `in` is the span a record sits under
(`turn>tool`), and `ev` says which part spoke: `pi::session` `pi::loop`
`pi::wire` `pi::tool` `pi::edit` `pi::bash` `pi::compact` `pi::keys`, plus
`pi::span` for the record that closes a span and carries its `dur_ms`. `--log debug` widens the fields, so
patches and tool arguments arrive whole rather than clipped to a kilobyte.
`--log trace` adds the request bodies themselves — hundreds of kilobytes a
turn, which is why they sit a level below everything else — and the
dependencies' own accounts, which is a lot of hyper. `--log off`
writes nothing; `PI_LOG` sets it for good.

Journals are as sensitive as transcripts — prompts, paths, file contents — and
kept the same way: `0600`, outside the workspace, dropped after two weeks. No
credential is written: keys travel in headers, which are never recorded, and a
field named for one is replaced by a fingerprint of it.

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

**A skill is also a command.** `/commit` runs the one called `commit` — no
prefix, because the name is what you know it by and a namespace only earns its
keep when something else wants the word. The instructions arrive as a message
you could have typed, so the model has them without spending a turn fetching
them, and anything after the word goes in below as what they are being applied
to. A built-in wins the collision: a repository contributes skills, and one
that could take `/new` away from the session it would otherwise start is a
checkout redefining the terminal — the skill stays loadable by name and the
startup notice says so. `/help` lists the two halves apart, since with no
prefix the word alone does not say which it is.

It reads the same one-shot: `pi "/commit fix the tests"` hands over the same
instructions to a run that answers once. Only the skills do — `/new` and the
rest operate on a session and there is none here, and any other word starting
with a slash is left as prose, because `pi "/usr/bin is missing"` is a prompt
and refusing it to catch a typo is the worse trade.

## What it does

**Tools.** `read` `write` `edit` `glob` `grep` `bash` `todo` `skill`.
Each declares a tier — read, write or exec — and `--tier` caps the run. Every
path is resolved against the workspace root through the deepest existing
ancestor, so a symlink cannot walk out. `bash` gets its own process group and a
SIGTERM-then-SIGKILL timeout.

**The plan** lives beside the conversation, not in it. `todo` records it and
answers with a count; the list itself rides every request as a note that is
recomputed from the stored plan and never written down. A plan in the
transcript is one copy per call, each stale the moment the next lands, and all
of them stated as fact.

**Edits** are line-anchored patches with content-hash anchors, applied against
original line numbers so an earlier hunk never shifts a later one. A stale
anchor is refused rather than applied to the wrong place. Concurrent edits to
one file serialize per path — otherwise both pass their tag check and one
change disappears silently.

**Compaction** is a ladder, cheapest rung first: supersede a read that a later
read replaced, omit an uneventful result, age one out, take the bulk of a tool
call's arguments once it has run, and only then summarize before dropping. What
it drops is a *round* — a question and everything that answered it — because
taking the answer alone left the question standing with nothing after it.

The session is append-only; compaction writes a *record* of what it dropped and
the model's view is derived from it, so the history that made the session worth
reading survives. What the model is sent and what a person reads are different
projections of the same list: compaction is the model losing sight of the
conversation, not you.

**Failures** are classified before they are retried. A spent quota and a
throttle both arrive as HTTP 429 and only the message text separates them —
retrying the first burns money. An overflow refusal usually names the real
window, so the correction is read out of it rather than guessed.

**A stuck model is named, because nothing else stops it.** There is no turn
cap: a run ends when the model stops, when you interrupt it, or when the
transport gives up. So a call that comes back byte-identical is told so — on
the third for an answer, since a re-read after compaction is legitimate, and on
the second for a refusal, since nothing legitimate re-sends a call that was
just refused. Repeated refusals are how most sessions actually die: a model
that cannot get a tool's arguments right will keep getting them wrong the same
way until it is told so.

**Token counts** come from the provider where it reports them and from our own
count where it does not — or where what it reports cannot be true. A proxy that
answers a thirty-thousand-token transcript with an input count of two hundred,
and no caching of any kind to explain it, is not tokenizing differently; taking that
figure at face value turns the running cost into fiction. A `~` marks which
half of a count is ours. A count we made is not the provider's, and a cost
derived from it is not a bill.

## Layout

| crate | | |
|---|---|---|
| `brain` | 2.7k | messages, wires, streams, faults, estimates |
| `agent` | 3.9k | the turn loop, compaction, the session |
| `tools` | 3.8k | the tool set and the tiered workspace gate |
| `cli` | 6.2k | terminal, config, sessions, the journal |
| `hashline` | 1.2k | the patch format — pure, no IO |
| `syntax` | 0.4k | tree-sitter outlines for eight languages |

~18k lines, 339 tests. `cargo test` runs everything; `cargo clippy
--all-targets` is expected to be silent.

## Not built

MCP, subagents, LSP, message-level cache breakpoints, session branching
(the log carries ids for it; nothing uses them yet), `Ctrl-Z` suspend.
