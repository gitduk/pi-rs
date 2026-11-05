# A session

| Input                              | Does                                          |
| ---------------------------------- | --------------------------------------------- |
| `/new`, `/resume [id]`, `/name`    | start, switch, label a session                |
| `/rewind <said>`, `esc esc`        | go back to something you said                 |
| `/model [name]`                    | list the models, or move the session to one   |
| `/effort [level]`                  | `off` `low` `medium` `high`, from now on      |
| `/worktree [name]`, `/worktree rm` | work in `<repo>.worktrees/<name>`             |
| `/mv <dir>`                        | move this session and its background commands to another directory |
| `/compact [focus]`                 | summarize everything but the working tail     |
| `/<tool> [key=value…]`             | run a named tool. Background tools return immediately |
| `/mcp`, `/mcp restart [name]`      | list MCP servers and tools, or reconnect      |
| `/settings`                        | edit the project's config                     |
| `/edit [file]`                     | edit a prompt, an `AGENTS.md`, or memory      |
| `/status`, `/keys`, `/help`        | show session paths and spend, bindings, or commands |
| `/exit`                            | exit pi, as with `ctrl+d`                     |
| `/content`                         | show current system blocks, their sources, and tool definitions |
| `/<skill> [args]`                  | run a skill (also one-shot: `pi "/commit"`)   |
| `!<command>`                       | run a shell command and record its output     |
| `!!<command>`                      | start one in the background and keep typing   |
| `@path`                            | complete a workspace file                     |
| click a code block's label         | copy the code, or the source of a diagram or table |
| drag, double or triple click       | copy the text, a word, or a line              |
| `ctrl+v`                           | paste a clipboard image as `[Image #n]`       |

Slash completion lists tools directly (for example, `/wechat`), then offers
their `key=value` arguments. Descriptions mark tools with `[tool]` and skills
with `[skill]`. Existing commands keep their names when a tool has the same
name. There is no `/tool` entry point.

In Vim Normal mode, `ctrl+k` focuses the last background command above the input.
`ctrl+k` and `ctrl+j` move between commands.
`ctrl+j` below the last command or `esc` returns to the draft at its original
cursor position. pi highlights the selected command.

- `x` stops the selected command.
- `r` restarts the selected command.
- `X` stops every running command in this workspace.
- `enter` expands or folds the selected command's output.

Stopped rows stay available while focused so you can restart them.
Completion lists and reading pages keep their own keys.
Insert mode keeps its editing keys.
`/keys` includes this help.

While a model reply is generating, completed background results and additional
messages you type wait for that reply to finish. After its tool calls complete,
they enter the next model request in the same run. A reply with no tool calls
also continues when messages are waiting. Generation is never interrupted.
Waiting messages show under the status line, and `up` takes them back. Typed
messages use a separate queue, so commands waiting for the run to end do not
hold them back. Remote inputs continue to wait for their own run.

A `!` command waits for the run to release the transcript.
It then runs to completion without a timeout.
A `!!` command uses the same background executor as the model's background
commands. It returns an ID immediately, without waiting for the transcript or
command completion. You can start it while a reply is generating.
Its output reaches the model when the command ends, together with other waiting
results.

Only one pi can open a session at a time.
