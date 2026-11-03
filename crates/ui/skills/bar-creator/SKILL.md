---
name: bar-creator
description: Create, edit, or troubleshoot pi's bar.rs script to customize the bar layout, parts, styles, or refresh behavior.
---

# Customize the bar

The cargo script `<pi_home>/bar.rs` defines the bar under the input line.
Use the directory in the `<pi_home>` block.
This directory is `$PI_HOME` when set, otherwise `~/.pi`.
pi runs the script with `cargo +nightly -Zscript`.
The script reads pi's state as JSON on stdin and prints a JSON layout.
The state fields are `model`, `effort`, `tier`, `running`, `root`, `worktree`, `ctx`, `spent`, `alerts`, `update`, and `released`.

## Read the script and example

Before editing an existing script, read `<pi_home>/bar.rs`.
Before creating a new script, read this skill's `bar.rs` example.
If any instruction or customization method is unclear, read the example before searching elsewhere.
This also applies when the user already has a script.
To read the example, call `skill` with `{"name": "bar-creator", "file": "bar.rs"}`.
This skill and its example describe the interface for ordinary customization.
pi's source code is not required.

Without this file, the bar shows `tabs`, `model`, and `spent` on the left.
It shows `update` and `flash` on the right.
pi runs the script again whenever its state changes.
If you set `refresh`, pi also runs it every `refresh` seconds.

## Define the layout

A layout contains `lines`.
Each line has `left` and `right` items and an optional `sep`.
The default separator is a muted ` · `.
Use one of these item formats:

- A part's name.
- `{"part": "model", "style": "heading"}` to style a part.
- `{"text": "custom text", "style": "muted"}` for the script's text.
- `{"group": [...]}` for adjacent items without separators.

A group can contain text and parts other than `tabs`.
Groups cannot nest.
If a group contains parts, pi hides the entire group when all those parts are empty.
This also hides the group's fixed text, icons, and spaces.
Fixed text does not keep such a group visible.
A group containing only text shows that text normally.

A style can use a theme name: `muted`, `heading`, `emphasis`, `code`, `input`, `ok`, `err`, `add`, or `del`.
It can also use an explicit style, such as `{"color": "#e5c07b", "sgr": ["bold"]}`.
pi omits an empty part and its preceding separator.

| Part        | Shows                                                     |
| ----------- | --------------------------------------------------------- |
| `tabs`      | the checkouts, folded to the room the row leaves          |
| `flash`     | the answer to the last key, for a moment                  |
| `model`     | the model                                                 |
| `effort`    | the thinking effort, omitted when off                     |
| `tier`      | the tool tier                                             |
| `update`    | a newer release or an installation that replaces the running pi |
| `spent`     | what the session has cost so far                          |
| `cost`      | what the last run cost                                    |
| `elapsed`   | how long the last run took                                |
| `in_out`    | its tokens in and out                                     |
| `speed`     | its output tokens per second                              |
| `cache`     | how much input the provider cached                        |
| `ctx`       | the context used, against the window                      |
| `compacted` | how often pi compacted the session                        |
| `queued`    | lines waiting for the run to end                          |
| `worktree`  | the worktree in front                                     |

The parts from `cost` through `worktree` also appear in the status line.
They describe the active run or the last completed run.

## Add a label or icon to a part

Put the text and part in one group so they appear and disappear together:

```json
{"group": [
  {"text": "⚡ ", "style": "muted"},
  {"part": "flash", "style": "muted"}
]}
```

When the `flash` slot has content, pi shows the icon, a space, and the content.
When the slot is empty, pi hides the entire group.
pi supplies part content when it draws the bar.
The script specifies the part's position and style.
The script does not need `flash` content on stdin.
The script does not need a conditional to hide this icon.
Use the same pattern for a label before `spent` or `effort`.
Use the user's chosen icon or one consistent with the existing script.
Investigate fonts or terminal configuration only when the user reports a glyph rendering problem.

## Check the script

Print only the JSON layout to stdout.
Send diagnostics to stderr.
Run the script with representative JSON on stdin before installing it.
For the supplied example:

```sh
printf '%s\n' '{"root":"/tmp/project","running":false}' | cargo +nightly -Zscript --quiet /path/to/bar.rs
```

Also check `running: true` when the script changes its layout during a run.
Use `refresh` for facts outside pi, such as a clock or repository status.
Omit `refresh` or set it to `null` when the script uses only pi's state.
If execution fails or the output is invalid, pi restores the default layout.
pi retries the script later.
pi reruns an existing script when its modification time changes.
If pi started without a script, restart pi after creating the file.
