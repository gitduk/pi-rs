---
name: pi-extend
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
  identifier, an inherited variable or `.env` already has it (`PATH` stays
  `PATH`), or the value is over 64 KiB. Read stdin when any of those can happen.
- A key or other value every script shares goes in `.env` beside the scripts,
  one `NAME=value` a line, and reaches each script as an environment variable;
  `bash` never sees it. Never write a secret into a script: read it from there.
- Output: stdout is the answer. Exit non-zero to fail; stderr is then the
  error the caller sees. stderr on success is dropped.
- It runs in the workspace root, only at the exec tier, one call at a time,
  with a 300 s limit until it detaches (next section). Large output is cut
  and spilled to a file like bash's.
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

## A tool that outlives its call

A long command needs no tool: `bash` with `background` runs it apart from
the turn and brings its exit and output back. Write a tool that detaches when
the work keeps reporting — a watcher, a crawler that hands back batches, a
bridge to a chat where a person talks to this session.

Such a script talks to pi over fd 3, one JSON object a line; `PI_EVENTS_FD`
is `3` when pi is listening. A script that never writes fd 3 is a plain tool.

| It writes | Meaning |
|---|---|
| `{"status": "..."}` | how far it has got: the call's progress, then its row above the bar |
| `{"detach": "..."}` | the call returns now with this text; the script runs on as a job |
| `{"result": "..."}` | after detaching: one result, a turn of its own on its checkout |
| `{"notice": "..."}` | a line on the screen only, which the model never reads: a QR to scan |
| `{"input": "..."}` | a person's words, sent to the model as written, never run as a command |
| `{"interrupt": true}` | stop the turn running on its checkout, as esc does |

For each turn an `input` opened, pi writes back on the same fd
`{"started": true}`, then `{"reply": "..."}`: the whole answer, ended with
`(stopped)` or `(failed: …)` when it did not finish. Every input gets one.

```python
#!/usr/bin/env python3
# ---
# description = "Watch a URL and report each time its content changes. Runs in the background until `jobs` stops it."
# [args]
# url = "the page to watch"
# ---
import hashlib, json, os, sys, time, urllib.request

if os.environ.get("PI_EVENTS_FD") != "3":
    sys.exit("needs pi to keep it running")
url = json.load(sys.stdin)["url"]
fd3 = os.fdopen(3, "w")
def say(**m):
    fd3.write(json.dumps(m) + "\n"); fd3.flush()

say(detach=f"watching {url}; each change comes back as a turn")
seen = None
while True:
    digest = hashlib.sha256(urllib.request.urlopen(url).read()).hexdigest()
    if seen and digest != seen:
        say(result=f"{url} changed")
    seen = digest
    say(status=f"checked {time.strftime('%H:%M')}")
    time.sleep(300)
```

- Detach first, before anything slow: until then the call holds the turn and
  the 300 s limit applies. After it, no limit: the job runs until it exits, a
  `jobs` stop (SIGTERM, then SIGKILL), or pi exits, and takes its process
  group along each time.
- `result`, `input` and `interrupt` count only after `detach`.
- Its stdout at exit is its last result; a non-zero exit adds stderr.
- What it saves between runs, it keeps itself, under `$PI_HOME`. A login
  token is written readable by its owner alone.
- `input` is a person's voice: use it only for words a person typed to this
  session. Data the work found goes back as `result`.
- Only a run that can keep jobs offers detaching; elsewhere — a one-shot
  `pi "..."`, a subagent — a detach is refused and the script ended.
- Try it by hand with fd 3 sent to the terminal:
  `echo '{"url":"https://example.com"}' | PI_EVENTS_FD=3 python3 <home>/tools/watch 3>&1`.

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

- `~/.pi/settings.toml` serves everywhere; a project's `.pi.toml` adds servers
  for that checkout alone, and pi names them under the banner at startup.
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
