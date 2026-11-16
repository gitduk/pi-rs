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
| `~/.pi/statusline.json`                  | the layout of the statusline rows          |
| `~/.pi/statusline.d/`                    | named scripts the layout may draw a value from |
| `~/.pi/sessions/…/<id>/statusline.json`  | that session's last run's numbers, so a resume opens with them |
| `~/.pi/sessions/<project>/<session>/session.jsonl`    | one session's conversation and request state |
| `~/.pi/sessions/<project>/<session>/`    | claims and session artifacts |

At startup, pi deletes sessions with no activity for 90 days.
It also deletes pasted images with no repeated paste for 90 days.
`keep_days` in `settings.toml` changes this interval. `0` keeps everything.

Diagnostics are off by default. `PI_LOG=debug pi` writes one process log to
`~/.pi/logs/<start-time>-<pid>.jsonl` and prints its path. `PI_LOG=trace pi`
adds detail. Files use mode `0600` and stop recording at 64 MiB. They stay
independent of session switching and retention. Clear diagnostic files manually.
Complete conversations and tool content stay in session archives.

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
