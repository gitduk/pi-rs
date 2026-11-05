---
name: pi-usage
description: Answer how to drive pi — the commands and keys a session takes, how background commands, MCP servers, scripts, hooks and skills behave, what reaches the model, and where its files and memory live. Use when asked what a command or key does, why a tool or a run behaved as it did, or where pi keeps something.
---

# Driving pi

Read the page the question is about. A flag or command no page names: `pi --help`
lists the flags.

- A session: commands, keys, queues, `!` and `!!`:
  `skill(name: "pi-usage", file: "references/session.md")`.
- Tools: built-ins, MCP, shell and background, scripts and their events,
  questions, hooks, skills:
  `skill(name: "pi-usage", file: "references/tools.md")`.
- What reaches the model: images, web access, repeated output, compaction:
  `skill(name: "pi-usage", file: "references/context.md")`.
- Files: what each path holds, retention, memory, the journal:
  `skill(name: "pi-usage", file: "references/files.md")`.

The README says what pi is and what it may reach; these pages say what it does
while it runs. When a page and the code disagree, the code is right: say where
the page is wrong instead of following it.
