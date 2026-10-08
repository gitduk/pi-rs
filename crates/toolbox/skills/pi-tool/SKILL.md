---
name: pi-tool
description: Give pi a capability it lacks — write it a tool (a script), connect an MCP server, or write a skill, all in pi's home. Use when the work needs a tool, server or skill that does not exist yet, or when asked to add, write or fix one.
---

# Extending pi

pi's home is the `<pi_home>` path in your prompt (`$PI_HOME`, else `~/.pi`).
It is read live: a tool or skill written there is offered from your next turn,
with no restart. Judge what you have by the tools offered now.

## A tool

One file in `tools/`; the file stem is the tool's name, so it takes only
a-z, A-Z, 0-9, `-` and `_`. Two kinds:

- **Any script** whose first line is a `#!` — shell, Python, Node, anything
  installed. Its interface is TOML in a comment block right after that line,
  between two `---` lines written in the script's own comment style. Prefer
  this: nothing to compile, and no toolchain beyond the interpreter.
- **A Rust cargo script**, `<name>.rs`, when the work wants Rust or a crate.
  Its interface is the `---` frontmatter cargo reads. It needs
  `cargo +nightly`, and the first call compiles, so it is slow.

Any other file in `tools/` is skipped without a word.

```sh
#!/usr/bin/env bash
# ---
# description = "Count the lines of a file. Answers with the number alone."
# [args]
# file = "path of the file to count, relative to the workspace"
# ---
# Every declared argument is also an environment variable of its name.
wc -l < "$file" || { echo "cannot read $file" >&2; exit 1; }
```

```rust
#!/usr/bin/env cargo
---
[package]
edition = "2024"
description = "Count the lines of a file. Answers with the number alone."

[package.metadata.pi.args]
file = "path of the file to count, relative to the workspace"
---

fn main() {
    let file = std::env::var("file").expect("file");
    match std::fs::read_to_string(&file) {
        Ok(text) => println!("{}", text.lines().count()),
        Err(e) => {
            eprintln!("{file}: {e}");
            std::process::exit(1);
        }
    }
}
```

- The header is required, and so is its description: `description` in a
  script's, `[package] description` in Rust's. A script missing either, or
  misnamed, shows up in the tool list as not usable, its description saying
  what is wrong; once fixed it is offered as itself from the next turn.
- The description is all a caller ever reads of the tool. Say what it does and
  what the answer looks like.
- Each `name = "what it is"` under `[args]` (Rust: `[package.metadata.pi.args]`)
  declares one argument. Every argument is a required string.
- Input: the call's full JSON on stdin, always. Each argument is also an
  environment variable of the same name, except when the name is not a valid
  identifier, an inherited variable already has it (`PATH` stays `PATH`), or
  the value is over 64 KiB. Read stdin when any of those can happen.
- Output: stdout is the answer. Exit non-zero to fail; stderr is then the
  error the caller sees. stderr on success is dropped.
- It runs in the workspace root, only at the exec tier, one call at a time,
  with a 300 s limit. Large output is cut and spilled to a file like bash's.
- The `#!` line is read by pi, not the kernel: no execute bit is needed.
- Rust crates go under `[dependencies]` in the frontmatter.

Steps:

1. Check the name is free: a built-in tool or an earlier script keeps it.
2. Write the file.
3. Run it once by hand the way pi will, then a failing case, and check the
   error reads well: `echo '{"file":"Cargo.toml"}' | file=Cargo.toml bash
   <home>/tools/count.sh` (Rust: `cargo +nightly -Zscript --quiet` instead of
   `bash`).
4. Call it as a tool on your next turn.

## An MCP server

When the capability already exists as an MCP server, connect it instead of
writing it: add a section to `settings.toml` in pi's home. pi notices the save
within a second, starts the server, and its tools join your list as
`<server>__<tool>` once it has listed them.

```toml
[mcp.github]                       # a-z, 0-9, - and _
command = "github-mcp-server"      # stdio: a program and its arguments
args    = ["stdio"]
env     = { GITHUB_TOKEN = "$GITHUB_TOKEN" }   # a value that is `$NAME` reads pi's environment

[mcp.docs]
url     = "https://example.com/mcp"            # or Streamable HTTP
headers = { Authorization = "$DOCS_AUTHORIZATION" }  # the whole value: "Bearer …"
```

- Only your own `settings.toml` may name servers; a project's `.pi.toml` that
  does is refused.
- A server that will not start shows up as `<server>__unavailable`, its
  description saying why; the user's `/mcp` lists every server and its tools,
  and `/mcp restart` reconnects them.
- Check the server's own documentation for its command and the variables it
  needs. Never write a secret into the file: name the variable with `$`.

## A skill

A directory, `skills/<name>/`, holding a `SKILL.md`: YAML frontmatter, then
the instructions.

```markdown
---
description: What it is for and when to use it — all a caller reads before loading it.
---

The instructions, followed when the skill is loaded.
```

- `description` is required; without it the skill is skipped. `name` is
  optional and defaults to the directory's name: a-z, 0-9, `-` and `_`.
- It is offered through the `skill` tool, and to the user as `/<name>`.
- Other files in the directory are reached with
  `skill(name: "<name>", file: "<path>")`; mention them in the instructions.
- A skill in `skills/` with the name of a built-in one replaces it.
