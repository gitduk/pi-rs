---
name: pi-extend
description: Add a missing capability to pi through a script tool, an MCP server, or a skill in pi's home. Use when the task needs a missing tool, server, or skill, or asks to add, write, or fix one.
---

# Extending pi

The `<pi_home>` block specifies pi's home.
The path is `$PI_HOME` when set, or `~/.pi` otherwise.
pi reads this directory live. A new tool or skill becomes available on your next turn without a restart.
Use the tools currently offered to determine which capabilities are available.

## A tool

Create one file in `tools/`. Its file stem becomes the tool name.
Use only a-z, A-Z, 0-9, `-`, and `_` in the name.

Choose one of these formats:

- **An interpreted script** with a `#!` first line, using an installed interpreter such as shell, Python, or Node.
  Put its TOML interface in a comment block immediately after that line.
  Enclose the interface between two `---` lines in the script's comment style.
  Prefer this format because it needs no compilation or toolchain beyond the interpreter.
- **A Rust cargo script**, `<name>.rs`, when the work requires Rust or a crate.
  Put its interface in the `---` frontmatter that cargo reads.
  This format requires `cargo +nightly`. The first call compiles the script and can be slow.

pi silently skips files in `tools/` that use neither format.

For a tool specific to this checkout, use `.pi/tools/` at the repository root.
Outside a repository, use `.pi/tools/` in the workspace.
pi reads this directory live too. It has its own `.env`.
If both directories contain the same tool name, pi offers the checkout's tool.
It skips the home tool and reports the conflict.

```sh
#!/usr/bin/env bash
# ---
# description = "Count the lines of a file. Return only the number."
# [args]
# file = "Path of the file to count, relative to the workspace."
# ---
# Every declared argument is also an environment variable of its name.
wc -l < "$file" || { echo "cannot read $file" >&2; exit 1; }
```

```rust
#!/usr/bin/env cargo
---
[package]
edition = "2024"
description = "Count the lines of a file. Return only the number."

[package.metadata.pi.args]
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

### Interface and execution

The header and description are required.
Use `description` for an interpreted script, or `[package] description` for Rust.
A missing or invalid header, description, or name produces an unusable tool entry that explains the problem.
After you fix the problem, the tool becomes available on your next turn.

The description is the caller's only tool documentation.
State what the tool does and how it formats its answer.

Each `name = "description"` under `[args]` declares one string argument.
For Rust, use `[package.metadata.pi.args]`.
Arguments are required unless their descriptions start with `optional:`.

The script always receives the full call as JSON on stdin.
Each argument also becomes an environment variable of the same name, except when:

- The name is not a valid environment variable identifier.
- An inherited variable or `.env` already defines that name.
- The value exceeds 64 KiB.

Read stdin when any exception can apply. An argument cannot replace an inherited variable such as `PATH`.

Put shared keys and other shared values in `.env` beside the scripts.
Use one `NAME=value` assignment per line.
Scripts receive these values as environment variables. The `bash` tool does not receive them.
Never put a secret in a script. Read it from the environment.

Write the answer to stdout.
Exit with a nonzero status to fail. On failure, the caller receives stderr as the error.
On success, pi discards stderr.

Scripts run in the workspace root at the exec tier, one call at a time.
A call has a 300-second limit until the script detaches.
Large output is truncated and saved to a file, as with bash.
pi reads the `#!` line itself. The file needs no execute permission.
For Rust scripts, put crates under `[dependencies]` in the frontmatter.

### Add and check a tool

1. Check that the name is available.
   A built-in tool takes precedence over a new script with the same name.
   An existing script also keeps its name.
   pi reports the conflict and skips the later script. Choose another name.
2. Write the file.
3. Run a successful case manually, using the same input and environment pi provides.
   Then run a failing case and check its error message.
   Example: `echo '{"file":"Cargo.toml"}' | file=Cargo.toml bash <home>/tools/count.sh`.
   For Rust, replace `bash` with `cargo +nightly -Zscript --quiet`.
4. Call the tool on your next turn.

## A tool that outlives its call

For a long command, use `bash` with `background`.
It runs independently of the turn and reports its exit status and output.
Write a tool that detaches when the work needs repeated reports.
Examples include watchers, crawlers that return batches, and bridges that relay a person's messages to this session.

A script sends events to pi through fd 3, one JSON object per line.
When pi listens, `PI_EVENTS_FD` is `3`.
A script that never writes to fd 3 remains a plain tool.

| Event | Meaning |
|---|---|
| `{"status": "..."}` | Report progress. During the call, this is call progress. After detach, it is job status. Each new status replaces the previous status. |
| `{"detach": "..."}` | Return this text from the call immediately. Continue running as a job. |
| `{"result": "..."}` | After detach, deliver one result as a separate turn in the job's checkout. |
| `{"input": "..."}` | Deliver a person's words to the model unchanged. Do not execute them as a command. |
| `{"interrupt": true}` | Stop the active turn in the job's checkout, as a user stop would. |
| `{"follow": true}` | Receive replies for all subsequent turns in the job's checkout, including turns opened by other inputs. |

For each turn opened by `input`, pi writes `{"started": true}` back through the same fd.
It then writes `{"reply": "..."}` with the complete answer.
An incomplete answer ends with `(stopped)` or `(failed: …)`.
Each input receives one reply.
After `follow`, the script also receives one reply for every other turn in its checkout, without duplicates.

```python
#!/usr/bin/env python3
# ---
# description = "Watch a URL and report each content change. Run in the background until stopped."
# [args]
# url = "The page to watch."
# ---
import hashlib, json, os, sys, time, urllib.request

if os.environ.get("PI_EVENTS_FD") != "3":
    sys.exit("needs pi to keep it running")
url = json.load(sys.stdin)["url"]
fd3 = os.fdopen(3, "w")
def say(**m):
    fd3.write(json.dumps(m) + "\n"); fd3.flush()

say(detach=f"watching {url}; each change arrives as a turn")
seen = None
while True:
    digest = hashlib.sha256(urllib.request.urlopen(url).read()).hexdigest()
    if seen and digest != seen:
        say(result=f"{url} changed")
    seen = digest
    say(status=f"checked {time.strftime('%H:%M')}")
    time.sleep(300)
```

### Job lifetime and channels

Detach before starting slow work.
Until detach, the call blocks the turn and the 300-second limit applies.
After detach, there is no timeout.
The job runs until it exits, is stopped, or pi exits.
Stopping sends SIGTERM, then SIGKILL. Job termination also ends its process group.

The events `result`, `input`, `interrupt`, and `follow` take effect only after `detach`.
At exit, stdout becomes the job's final result. A nonzero exit also includes stderr.

After detach, `status` and stderr are for the user. The model receives neither during execution.
Use `status` for what the user needs to know or do now, such as scanning a QR code.
Use stderr for supporting content, such as the QR code itself.
Only a failing exit sends stderr to the model.

The script manages its own persistent state under `$PI_HOME`.
Restrict saved login tokens to owner access.

Use `input` only for words a person typed to this session.
Send data discovered by the job as `result`.

Only runs that support jobs allow detaching.
Other runs, including one-shot invocations and subagents, refuse detach and stop the script.

To check the event protocol manually, redirect fd 3 to the terminal:
`echo '{"url":"https://example.com"}' | PI_EVENTS_FD=3 python3 <home>/tools/watch 3>&1`.

## An MCP server

If the capability exists as an MCP server, connect that server instead of writing a tool.
Add a section to `settings.toml` in pi's home.
Within a second of the save, pi starts the server.
After listing the server's tools, pi offers them as `<server>__<tool>`.

```toml
[mcp.github]                       # a-z, 0-9, - and _
command = "github-mcp-server"      # stdio: a program and its arguments
args    = ["stdio"]
env     = { GITHUB_TOKEN = "$GITHUB_TOKEN" }   # $NAME reads pi's environment

[mcp.docs]
url     = "https://example.com/mcp"            # Streamable HTTP
headers = { Authorization = "$DOCS_AUTHORIZATION" }  # the complete value, including "Bearer "
```

`~/.pi/settings.toml` applies to every checkout.
A project's `.pi.toml` adds servers for that checkout alone.
pi reports those servers at startup.

A server that cannot start appears as `<server>__unavailable`.
Its description explains the failure. The user can list servers and their tools, or reconnect a server.

Check the server's documentation for its command and required variables.
Never put a secret in the file. Reference the environment variable with `$`.

## A skill

Create a directory at `skills/<name>/`.
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
For a checkout-specific skill, use `.pi/skills/` at the repository root.
Outside a repository, use `.pi/skills/` in the workspace.
pi reads this directory live too.
If both directories contain the same skill name, pi offers the checkout's skill.
