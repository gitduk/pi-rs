# Files

`PI_HOME` moves `~/.pi`.

| Path                                     | Holds                                      |
| ---------------------------------------- | ------------------------------------------ |
| `~/.pi/settings.toml`                    | providers, models, `[keys]`, `[theme]`     |
| `~/.pi/SYSTEM.md`                        | replaces the built-in system prompt        |
| `~/.pi/prompts/summarize.md`             | replaces the compaction summary prompt     |
| `~/.pi/prompts/subagent.md`              | replaces a subagent's own prompt           |
| `~/.pi/AGENTS.md`, `AGENTS.md`           | instructions: yours, a project's           |
| `~/.pi/skills/`                          | skills: a directory with a `SKILL.md`      |
| `.pi/skills/`                            | a project's skills                         |
| `~/.pi/memory/*.md`                      | memory: global, and one file per project   |
| `~/.pi/tools/`                           | script tools                               |
| `~/.pi/tools/.env`                       | initial environment values for scripts, mode `0600` |
| `.pi/tools/`                             | a project's script tools                   |
| `~/.pi/images/`                          | images pasted with `ctrl+v`                |
| `~/.pi/bar.rs`                           | a cargo script that lays out the bar       |
| `~/.pi/sessions/<project>/<session>/`    | the transcript and `journal.jsonl`         |

At startup, pi deletes sessions with no activity for 90 days.
It also deletes pasted images with no repeated paste for 90 days.
`keep_days` in `settings.toml` changes this interval. `0` keeps everything.

The journal records what the transcript does not: requests, timings, refusals,
retries. One JSON object per line, `0600`, kept two weeks. `PI_LOG` takes
`debug`, `trace` or `off`.

## Memory

pi records memory without a request.
When you correct it, state a preference, or settle a decision, the model writes a
line with `memory`.
The line goes to `~/.pi/memory/`, and pi shows it as the model writes it.
The next session or compaction includes that memory.

Memory has a total limit of 16 KiB.
pi refuses a change that would increase memory beyond this limit until the model
combines or removes lines. The files are plain Markdown.
`/status` lists their paths.
You can edit or delete them.
Subagents and runs with `--tier read`, `--tier net`, or `--no-context-files`
never write memory.
