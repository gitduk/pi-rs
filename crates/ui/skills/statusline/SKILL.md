---
name: statusline
description: Configure pi's statusline layout, fields, colours, and script values.
---

# Set the statusline

The statusline is the rows above and under the input line.
Its layout is a single JSON document kept at `<pi_home>/statusline.json`.
Use the directory in the `<pi_home>` block.
This directory is `$PI_HOME` when set, otherwise `~/.pi`.
Without that file, the default layout stands: `tabs`, `model` and `spent` on the
left, `update` and `flash` on the right.

Set it with the `statusline` tool.
Pass the whole document as `layout`, or `reset: true` to remove the file and go
back to the default.
The tool checks the document and answers with what it set.
pi reads the file again on the next second, so no restart is needed.

Do not read pi's source to answer a customization question.
This skill describes the interface.

## The document

```json
{
  "lines": [
    {"place": "top", "left": [{"field": "flash", "style": "emphasis"}]},
    {
      "left":  ["workspace", "branch", "diff", {"group": ["model", {"text": " "}, "effort"]}],
      "right": [{"script": {"command": "date +%H:%M", "every": 30}}],
      "sep":   {"text": "  ", "style": "muted"}
    }
  ]
}
```

`place` is `top` or `bottom`, and defaults to `bottom`.
A `top` line stands between the input line and the conversation; a `bottom`
line stands under the input line.
Every line keeps its row whether or not it has something to say, so the input
line never moves.

`left` starts at the left edge; `right` stands against the right edge.
`sep` stands between two items of one side, and defaults to a muted ` · `.
A missing `style` is muted.

## Items

| Written as | Shows |
|---|---|
| `"model"` | one field, by name |
| `{"field": "cache", "style": "ok"}` | one field in a colour |
| `{"text": " \| ", "style": "muted"}` | fixed text |
| `{"group": [item, …]}` | items joined without `sep`; the whole group disappears when each field in it has nothing to say |
| `{"unless": "tabs", "items": [item, …]}` | items drawn only while that field has nothing to say |
| `{"when": "diff", "items": [item, …]}` | items drawn only while that field has something to say |
| `{"script": {"command": "…", "every": 30}}` | a value an outside command prints |
| `{"script": "weather"}` | a value `<pi_home>/statusline.d/weather` prints |

`tabs` cannot sit inside a group: it folds when the row runs out of width, and
it draws nothing while the one checkout is all there is.

## Fields

A field draws its own text. Treat that text as output for the user, not as a
format to copy. To show the same facts in another shape, ask a script for the
parts you want.

`tabs` `flash` `model` `effort` `tier` `update` `spent` `state` `session`
`workspace` `branch` `diff` `elapsed` `in_out` `speed` `cache` `cost` `ctx`
`compacted` `queued` `worktree`

- `workspace` shows the checkout directory's name.
- `branch` shows the branch the checkout is on; `diff` shows the lines it has
  added and removed against `HEAD` in the theme's diff colours, and says
  nothing while the worktree is clean. Both are empty outside a repository.
- `state` — `idle`, `working`, `compacting`, `stopping`, `waiting` once a turn
  has gone quiet, or a retry it waits out. Always says something.
- `session` — what the user calls this session, else the first thing it was
  asked, cut short; absent until one of the two exists.
- `elapsed` — how long the run in flight has taken, and how long the run that
  last ended took once the row is back to idle.
- `spent` — what the session has cost so far; `cost` — what the run in flight,
  or the one that last ended, cost.
- `cache` `ctx` `cost` are kept in the session's own `statusline.json`, so a
  resumed session opens with them rather than blank.

## Styles

`muted` `heading` `emphasis` `code` `input` `ok` `err` `add` `del` take their
colours from the theme, so a theme change carries.
An SGR number such as `"34"`, or `{"color": "#5c9cf5", "sgr": ["bold"]}`, sets
colours outright.
`{"tone": "code", "sgr": ["bold"]}` keeps the theme's colour for that tone and
adds the emphasis.

Give a script's answer its colour through `style`. Do not write an SGR run into
the output. Only `style` follows a theme change. Use `add` for the lines added
and `del` for the lines removed.

## Scripts

A script item carries exactly one of `command` or `name`.

- `command` runs through `sh -c`, so a one-line answer needs no file.
- `name` runs `<pi_home>/statusline.d/<name>`, which must be executable. Write
  the file, make it executable, then name it here. The name takes no `/`, no
  `..`, and no leading `-`.
- `every` is the seconds between runs, at least 1, default 60.
- `timeout` is the seconds one run may take before it is killed, default 15.
  A script says how long it needs, so a slow one is not mistaken for a hung one.
- `style` colours its answer like any other item's text.
- `placeholder` stands in until the first answer, default `…`.
- Only the first line of output shows. SGR runs (`ESC[31m…ESC[0m`, a real byte 0x1b) colour it —
  the same vocabulary a theme writes, applied over `style` — and every other
  escape and control character is dropped.
- A blank or missing first line clears the cell regardless of the exit code.
  A script that cannot start, cannot be read, reaches its timeout, or exceeds
  the output limit before any visible text keeps its previous answer or placeholder.
- At most four scripts run at once, each one alone, and none of them blocks the
  row.
- A script is told `PI_SESSION_DIR`, the directory of the session it draws for,
  `PI_COLUMNS`, the row's width in cells, and `PI_FACTS`, one JSON object of
  what the row knows, the same values its fields draw from:
  - `model`, `provider`, `effort`, `tier`, `running`, `state`, `alerts`,
    `update`, `released`;
  - `session` — `id`, `name` (only once the user names it), `title` (that name,
    else the first thing it was asked), `started` in seconds since the epoch,
    `workspace`, and `spent`: `usage` (`input`, `output`, `cache_read`,
    `cache_write`) with `cost` in dollars, the whole session, children and
    compactions alike;
  - `run` — `elapsed` in seconds, `input`, `output`, `cache` as a percent,
    `cost`, `ctx` as `[used, window]`, `context_window`, `speed`, `compacted`,
    `queued`, `worktree`, `retry` (`attempt`, `wait` in seconds), and `outcome`
    (`ok`, `stopped` or `failed`) once a run ends;
  - `tabs` — every checkout on the bar: `name`, and `mark` (`front`, `done`,
    `failed`, `plain`, `unopened`);
  - `jobs` — background executions: `running`, `back` (ended with results
    waiting), `stopped`, `exited`;
  - `workspace` is the checkout directory's name, omitted when the path has no name.
  - `git` has `branch`, `added` and `removed`, and is omitted outside a repository.
- Optional values are `null` unless documented as omitted. `session` is omitted
  outside a lane.

A condition reads these values. Use `tabs` to keep a cell for the case where
only one checkout is on the bar. Use `git` to keep a cell empty outside a
repository. Use `run.outcome` to say how a run ended.

## When one part stands or falls with another

A field always draws its whole shape. When one part of a cell should appear
only under a condition, the condition goes in the layout: not in the field,
which would then have to know what another field says, and not in a script,
which has no answer for the first frames and leaves the cell empty until one
comes back.

```json
{"group": [
  {"field": "workspace", "style": {"tone": "code", "sgr": ["bold"]}},
  {"unless": "tabs", "items": [{"text": " "}, {"field": "branch"}]},
  {"when": "diff", "items": [{"text": " "}]},
  {"field": "diff"}
]}
```

`{"unless": "<field>", "items": […]}` draws its items only while that field
has nothing to say, and `{"when": "<field>", "items": […]}` only while it has
something to say. The row decides both from the facts it already holds, so no
run stands between the two and nothing waits to be told. Here the branch gives
way once `tabs` names the checkouts, which it does while there is more than
one. The space before the diff stands with the diff alone, so a clean worktree
leaves no gap.

Two jobs still belong to a script: a value the row cannot read for itself, and
a condition on something no field draws.

The statusline redraws from the last answer, so a slow script never stalls the
screen.
