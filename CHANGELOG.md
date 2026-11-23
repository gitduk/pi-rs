# Changelog

What changed for the person using pi, release by release. The format follows
[Keep a Changelog](https://keepachangelog.com/1.1.0/) and versions follow
[SemVer](https://semver.org/): a major version means the settings file, the
session store or the tool script protocol changed shape.

Releases before 3.0.0 are recorded in the commit history, not here.

## Unreleased

### Changed

- A pi directory's `.env` moved from `tools/` up to the directory holding it:
  `~/.pi/tools/.env` is now `~/.pi/.env`, and a checkout's `.pi/tools/.env` is
  now `.pi/.env`. A file left at the old path is read by nothing and masks
  nothing, so move it. The same file now also answers `$NAME` in the settings,
  after pi's own environment: a provider key can live there alone, with no
  `export` in the shell. The settings file's shape for a key changed, so this
  is a major version.

## 3.0.5 - 2026-11-20

### Changed

- A command reaches the shell as written: a leading `cd` into the workspace
  is no longer removed before it runs, and the system prompt names the
  workspace path in prose instead of a `<workspace path="…"/>` tag.

### Fixed

- A skill whose instructions outgrow one tool result is refused instead of
  truncated: the model no longer runs on half a body with nothing saying so,
  and the refusal names where the detail should go.

## 3.0.4 - 2026-11-20

### Added

- A `.env` variable whose last name segment is `KEY`, `TOKEN`, `SECRET`,
  `PASSWORD`, `PASSWD` or `CREDENTIALS` is masked whatever its value, since
  its own name already says what it is; four characters is the floor there
  where the shape rule still asks eight, and `/mask` files such a value as
  named rather than guessed.

### Changed

- The execution policy says to leave a long-lived command with the host's
  background execution, never `setsid`, `nohup` or `&`.

### Fixed

- A connection's `api_key` is masked only at eight characters or more: a stub
  key that a local endpoint ignores used to cover every word that held it, so
  the model was handed prose cut into tokens and quoted the pieces back.
- The `.env` shape rule counts characters rather than bytes, so a short
  multibyte value no longer passes on its byte length.

## 3.0.3 - 2026-11-19

### Added

- Secrets leave as tokens. Values from the config, a `secrets` list and the
  tool `.env` files are replaced on the way out — streamed deltas and tool
  arguments included — and put back on the way in, so a command the model
  writes still runs with the real key. Signatures, encrypted reasoning and
  image data go out untouched, and a token nobody issued is left as written.
  `/mask` lists what was masked, each value's ends while they stay a minority
  of it, and how often it left.

### Changed

- Recall belongs to the session that typed it, kept beside its transcript:
  `k` offers this conversation's lines and no other's, a resumed session comes
  back with them, and `/mv` carries them along. Older lists are dropped.

## 3.0.0 - 2026-11-17

### Breaking

- **A provider entry owns the connection.** Top-level `base_url`, `format`,
  `api_key` and `cache_control` are gone: each lives under `[provider.<name>]`,
  and a model is selected as `provider/model-id`. Settings moved from
  `.pi.toml` to `.pi/settings.toml`. The old connection keys are refused at
  load rather than reinterpreted.
- **Sessions are one file each.** 2.x wrote `<id>/session.json` beside a
  request journal; 3.0.0 writes `<id>/session.jsonl` and appends the request
  state to it. Sessions written by 2.x stay on disk but are not listed or
  resumed.
- **Tool scripts speak protocol revision 4 on fd 3.** A line carrying `id` is
  a request answered exactly once; a line without one is a notice never
  answered. `initialize` names the revision, and a mismatch refuses the script
  at startup instead of after a long turn. A script that never writes to fd 3
  is unchanged.
- **A statusline field can name the session.** `session` draws what the user
  called it, and until then the first thing it was asked, cut short, so a row
  can say `working notes-fix` rather than working in the abstract.
- **The statusline is a file, not a Rust script.** `~/.pi/bar.rs`, run with
  `cargo +nightly -Zscript`, is gone; `~/.pi/statusline.json` holds the rows and
  the `statusline` tool writes it. The `bar-creator` skill is `statusline`.
- **`PI_FACTS` is the row's own shape.** A script is told the same values the
  fields draw from, serialized from one struct: `provider`, `session` (`id`,
  `name`, `started`, `workspace`, `spent`), `run` (now with `context_window`,
  `retry` and `outcome`), `tabs`, and `jobs` are added, and a session's
  spending moved from the top-level `spent` to `session.spent`.
- **`/loop` is gone, and `loop_max_rounds` with it.** A tool holds a checkout's
  turns by speaking with `"more": true` on fd 3, files the round's note with its
  own `say`, and keeps the counter and the brakes itself.
- **Built-in skills changed.** `mcp-setup` is part of `pi-setup` now,
  `pi-usage` is new, and `skill-creator` is gone.
- **`subagent` runs in the background by default.** The call returns a
  `task_id` and the conclusion arrives on its own; pass `background: false`
  when the next action needs the result.
- **Two transcript row kinds are gone.** Notes and tallies no longer appear in
  the transcript: history holds what was said, the statusline holds the figures.
- **`/status` no longer carries the prompt budget.** It moved to `/context`.

### Added

- Models are discovered per provider connection from `<base_url>/models`, with
  the last good list kept on failure; `[provider.<name>]` maps capability paths
  and a pricing table, declares extra `[headers]`, and may name the header that
  carries a session id.
- `/context` ranks the system prompt, tool definitions and messages against the
  whole window, and inspects any of them on its own page with completion.
- A skill's header can declare workspace-relative globs; reading a matching file
  reminds the model to load it before making changes.
- `subagent` takes `model`, `tools`, `skills`, `max_turns` and `timeout_ms`, and
  shows the child's model in its progress and preview; the `models` tool lists
  the locally known catalog with limits, capabilities and prices.
- A background task outlives the turn that started it, a background command can
  drive a run from the insert-mode line, and both a queued input and a
  background result are admitted into the running turn.
- Statusline scripts are told `PI_SESSION_DIR`, `PI_COLUMNS` and `PI_FACTS`,
  colour their own text with SGR runs, and refresh as often as once a second.
- A call opens whole on its own page and in a fixed window from its bar, and a
  group of steps is counted by tool over a bar of its own.
- Run budgets are unified and `bash` can name a `timeout`; a `cd` into the
  directory a command already runs in is dropped.
- A session keeps its file views, so a resume still diffs; a resumed session
  keeps its own readings.
- Skill loads are counted across sessions.
- `pi --example` prints the shipped settings file.

### Changed

- Configuration and MCP servers are owned per workspace, so a reload in the
  background no longer disturbs the workspace in front.
- Model discovery refreshes every minute, keeping the `/model` page current.
- A chat endpoint that rejects `thinking` is retried once without it, and the
  omission is remembered per connection.
- The built-in prompt policy and tool guidance are smaller, with verification
  and authorization stated precisely once.
- The banner fades toward the terminal background, and the thinking glyph turns
  at half its earlier pace.

### Fixed

- A skill's reminder clears only when its instructions arrive whole, survives a
  compaction and a resume.
- A call's non-zero exit is painted in the error colour, and a result with rows
  keeps a line of its own.
- The statusline stays drawn under a mouse selection, and keeps the previous
  elapsed time on the row.
- The input's block stays still when a list opens, and prompt spacing survives
  waiting and wrapped lines.
- A cut preview ends at its "more" footer, and a fold keeps its way back when
  its lines are cut.
- A session whose model the config no longer has still opens.
- Provider catalog facts are kept across reloads and new lanes.
- A chat model's own thinking is replayed back to it.
