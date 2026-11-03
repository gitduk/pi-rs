---
name: bar-creator
description: Create, edit, or troubleshoot pi's bar.rs script to customize the bar layout, parts, styles, or refresh behavior.
---

# Customize the bar

The cargo script `bar.rs` in pi's home defines the bar under the input line.
Use the `<pi_home>` block for the home directory: `$PI_HOME` when set, otherwise `~/.pi`.
It runs with `cargo +nightly -Zscript`.
It reads pi's state as JSON on stdin and prints a layout.
The state fields are `model`, `effort`, `tier`, `running`, `root`, `worktree`, `ctx`, `spent`, `alerts`, `update`, and `released`.
Read `bar.rs` from this skill as a starting example.
Use `skill` with `name: "bar-creator"` and `file: "bar.rs"` to read it.
When updating an existing script, read that file before editing it.

Without this file, the bar shows `tabs`, `model`, and `spent` on the left.
It shows `update` and `flash` on the right.
pi runs the script again whenever its state changes.
If you set `refresh`, pi also runs it every `refresh` seconds.

A layout contains `lines`.
Each line has `left` and `right` items and an optional `sep`.
The default separator is a muted ` · `.
Use one of these item formats:

- A part's name.
- `{"part": "model", "style": "heading"}` to style a part.
- `{"text": "custom text", "style": "muted"}` for the script's text.
- `{"group": [...]}` for adjacent items without separators.
  pi omits the entire group when all its parts are empty.

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

## Check the script

Print only the JSON layout to stdout; send diagnostics to stderr.
Run the script with representative JSON on stdin before installing it.
For the supplied example:

```sh
printf '%s\n' '{"root":"/tmp/project","running":false}' | cargo +nightly -Zscript --quiet /path/to/bar.rs
```

Also check `running: true` when the script changes its layout during a run.
Use `refresh` for facts outside pi, such as a clock or repository status.
Omit it or set it to `null` when only pi's state is needed.
If execution fails or the output is not a valid layout, pi restores the default layout and retries later.
An existing script is rerun when its modification time changes.
If you create the file after pi has started without one, restart pi to discover it.
