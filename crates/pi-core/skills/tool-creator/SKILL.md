---
name: tool-creator
description: Create or repair registered pi tools and their script interfaces. Ordinary task scripts do not require registration.
---

# Create or fix a tool

A script is a code file that an interpreter or runtime executes.
A tool is a named interface that the model can call directly.
A tool can use a script as its implementation.
Ordinary task scripts do not need tool registration, even when you run them repeatedly.
Keep task scripts in the workspace.
Run them through bash.

For a reusable interface, define its purpose, inputs, and output before you write its implementation.
Pass the current task's data as arguments.
Do not embed that data in the implementation.

The `<pi_home>` block specifies pi's home.
pi uses `$PI_HOME` if you set it, or `~/.pi` otherwise.
pi reads tool directories during the session.
A new or updated tool appears in the next request to the model.
You do not need to restart pi.
Use the tools currently offered to determine which capabilities are available.

## Read the relevant references

Read these files through `skill(name: "tool-creator", file: "<file>")`:

- `interface.md`: before writing a tool's header, arguments, or output.
- `writing.md`: before writing descriptions or error messages.
- `background.md`: when a tool must continue after its call.
- `events.md`: when a script must report progress, deliver results, or ask the user a question.

If an instruction or implementation method is unclear, read the relevant reference before searching elsewhere.
The references include interface rules and examples.

## Implement a tool with a script

Choose the tool directory before creating the file.
The `<project_tools>` and `<pi_home>` blocks identify the tool directories in the system instructions.
These blocks specify which directory to use for each tool.
In the examples below, `<tools-dir>` means the directory you chose.

pi scans supported files in a tool directory for tool interfaces.
It registers those interfaces for subsequent model calls.
An invalid interface can produce an unusable tool entry.
Do not store task scripts, helper files, generated data, or temporary files in either tool directory.

Create one file in the selected directory.
The file name without its extension becomes the tool name.
Use only a-z, A-Z, 0-9, `-`, and `_` in the name.

Choose one of these formats:

- **An interpreted script** with a `#!` first line.
  Use an installed interpreter such as shell, Python, or Node.
  Put its TOML interface in a comment block immediately after that line.
  Enclose the interface between two `---` lines in the script's comment style.
  Prefer this format because it needs no compilation or toolchain beyond the interpreter.
- **A Rust cargo script**, `<name>.rs`.
  Use this format when the work requires Rust or a crate.
  Put its interface in the `---` frontmatter that cargo reads.
  This format requires `cargo +nightly`.
  The first call compiles the script and can be slow.

pi silently skips files in `tools/` that use neither format.

pi also reads the project tool directory during the session.
Each tool directory's `.env` sits one level up, beside `tools/`, not inside it.
The home directory's `.env` also answers `$NAME` in the settings.
If both directories contain the same tool name, pi offers the checkout's tool.
It skips the home tool and reports the conflict.

```sh
#!/usr/bin/env bash
# ---
# description = "Count the lines of a file. Return only the number."
# [required]
# file = "Path of the file to count, relative to the workspace."
# ---
# This example reads the file argument from its environment variable.
wc -l < "$file" || { echo "cannot read $file" >&2; exit 1; }
```

```rust
#!/usr/bin/env cargo
---
[package]
edition = "2024"
description = "Count the lines of a file. Return only the number."

[package.metadata.pi.required]
file = "Path of the file to count, relative to the workspace."
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

## Add and check a tool

For description-only edits, check the interface syntax and requested text.
Inspect the diff for unrelated changes.
Stop when these checks pass.

For new tools or behavior changes, use the following steps:

1. For a new or renamed tool, check that the name is available.
   A built-in tool takes precedence over a new script with the same name.
   An existing script also keeps its name.
   pi reports the conflict and skips the later script.
   If the name conflicts, choose another name.
2. Write the requested changes.
3. Run relevant success and failure cases with isolated fixtures when available.
   Use the input and environment pi provides.
   Check their output and error messages.
   Run live cases only when the task authorizes their effects.
   Report checks that cannot run with the available inputs or authorization.
   Example: `echo '{"file":"Cargo.toml"}' | file=Cargo.toml bash <tools-dir>/count.sh`.
   For Rust, replace `bash` with `cargo +nightly -Zscript --quiet`.
4. If verification requires a registered call, use the tool when it appears in the model's tool list.
