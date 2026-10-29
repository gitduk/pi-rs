---
name: skill-creator
description: Create or update skills for pi, including their instructions, helper scripts, and supporting files. Use when the task needs reusable guidance rather than a callable tool interface.
---

# Creating a skill

A skill provides reusable instructions and can include helper scripts.
Keep helper scripts inside the skill's directory and describe how to run them in `SKILL.md`.
Ordinary task scripts belong in the workspace and do not require a skill.

The `<pi_home>` block specifies pi's home: `$PI_HOME` when set, or `~/.pi` otherwise.
Use `skills/` inside that home for skills shared across projects.
For a checkout-specific skill, use the nearest `.pi/skills/` up to the repository root.
Outside a repository, use `.pi/skills/` in the workspace.
The run's write boundary still applies.

Create a `<name>/` directory under the selected skill directory.
Add a `SKILL.md` containing YAML frontmatter followed by instructions.

```markdown
---
description: State the skill's purpose and when to use it. The caller reads this before loading the instructions.
---

The instructions to follow after loading the skill.
```

The `description` field is required. Without it, pi skips the skill.
The `name` field is optional and defaults to the directory name.
Use only a-z, 0-9, `-`, and `_` in the name.

pi offers the skill through the `skill` tool.
Users can also invoke a skill by name.
Read other files within the skill directory with `skill(name: "<name>", file: "<path>")`.
Reference these files in the instructions.

A skill in `skills/` replaces a built-in skill with the same name.
pi reads skill directories live. A new or updated skill becomes available on the next turn without a restart.
If both directories contain the same skill name, pi offers the checkout's skill.
