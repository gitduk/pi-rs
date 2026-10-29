---
name: tool-creator
description: Create or fix reusable tools for pi, including their script interfaces and background events. Use when the model needs a named interface for direct calls. Ordinary task scripts do not need this skill.
---

# Creating a tool

A tool is a named interface that the model can call directly.
A script can implement a tool, but ordinary task scripts belong in the workspace and run through `bash`.
Running a script repeatedly does not require a new tool.
For a reusable interface, define its purpose, inputs, and output before writing its implementation.
Pass the current task's data as arguments instead of hardcoding it.

The `<pi_home>` block specifies pi's home: `$PI_HOME` when set, or `~/.pi` otherwise.
pi reads tool directories live. A new or updated tool becomes available on the next turn without a restart.
Use the tools currently offered to determine which capabilities are available.

## Implement a tool with a script

Choose the tool directory before creating the file.

- Use the project tool directory specified in the system instructions by default.
  This includes tools that depend on the project's code, data, paths, or configuration.
  Create the directory if it does not exist.
- Use `tools/` inside pi's home only when the user requests a tool shared across projects.

The run's write boundary still applies.
If that boundary blocks the project tool directory, report the limit.
Do not use pi's home as a fallback.
In the examples below, `<tools-dir>` means the directory selected above.

The home and project `tools/` directories register tools for subsequent model calls.
pi scans supported files here for tool interfaces.
An invalid interface can produce an unusable tool entry.
Keep task scripts, helper files, generated data, and temporary files outside these directories.

Create one file in the selected directory. Its file stem becomes the tool name.
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

pi reads the project tool directory live too. It has its own `.env`.
If both directories contain the same tool name, pi offers the checkout's tool.
It skips the home tool and reports the conflict.

```sh
#!/usr/bin/env bash
# ---
# description = "Count the lines of a file. Return only the number."
# [required]
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

### Interface and execution

The header and description are required.
Use `description` for an interpreted script, or `[package] description` for Rust.
A missing or invalid header, description, or name produces an unusable tool entry that explains the problem.
After you fix the problem, the tool becomes available on your next turn.

The description is the caller's only tool documentation.
State what the tool does and how it formats its answer.

Each `name = "description"` under `[required]` or `[optional]` declares one string argument.
For Rust, use `[package.metadata.pi.required]` and `[package.metadata.pi.optional]`.
Either table may be omitted. Omit both for a tool with no arguments.
A name cannot appear in both tables. Descriptions do not control whether an argument is required.

For example, declare a reminder's arguments as:

```toml
[required]
prompt = "What to do when the reminder comes due."

[optional]
after = "A delay: 30s / 10m / 2h / 1d."
every = "A period of at least 1m."
```

Required means the argument must be provided; the script validates its value, including empty strings.
The script supplies defaults for omitted optional arguments. Omission and an empty string are distinct.

The script always receives the full call as JSON on stdin.
Each argument also becomes an environment variable of the same name, except when:

- The name is not a valid environment variable identifier.
- An inherited variable or `.env` already defines that name.
- The value exceeds 64 KiB.

Read stdin when any exception can apply. An argument cannot replace an inherited variable such as `PATH`.

Put shared keys and other shared values in `.env` beside the scripts.
Use one `NAME=value` assignment per line.
Scripts receive these values as environment variables. Ordinary `bash` calls do not receive them. Background wrappers load them only for their subprocess.
Never put a secret in a script. Read it from the environment.

Write the answer to stdout.
Exit with a nonzero status to fail. On failure, the caller receives stderr as the error.
On success, stderr is excluded from the model result; background output still retains it for the user.

Scripts run in the workspace root at the exec tier, one call at a time.
A foreground script call has a 300-second limit. Background wrappers have no timeout.
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
   Example: `echo '{"file":"Cargo.toml"}' | file=Cargo.toml bash <tools-dir>/count.sh`.
   For Rust, replace `bash` with `cargo +nightly -Zscript --quiet`.
4. Call the tool on your next turn.

## A tool that outlives its call

For a long command, use `bash` with `background`.
It runs independently of the turn and reports its exit status and output.
For a named background tool, set `background = true` at the top of its shebang comment block,
or under `[package.metadata.pi]` in Rust frontmatter.
The named entry delegates to the same bash background executor, passing JSON on stdin
and loading the tool directory’s `.env` for its subprocess.
Examples include watchers, crawlers that return batches, and bridges that relay a person's messages to this session.

A script sends events to pi through fd 3, one JSON object per line.
When pi listens, `PI_EVENTS_FD` is `3`.
Background mode is declared in metadata. It does not depend on fd 3 events.

| Event | Meaning |
|---|---|
| `{"status": "..."}` | Report progress. In foreground mode, this is call progress. In background mode, it is command status. Each new status replaces the previous status. |
| `{"result": "..."}` | In background mode, deliver one result as a separate turn in the command's checkout. |
| `{"input": "..."}` | Deliver a person's words to the model unchanged. Do not execute them as a command. |
| `{"interrupt": true}` | Stop the active turn in the command's checkout, as a user stop would. |
| `{"follow": true}` | Receive replies for all subsequent turns in the command's checkout, including turns opened by other inputs. |

For each turn opened by `input`, pi writes `{"started": true}` back through the same fd.
It then writes `{"reply": "..."}` with the complete answer.
An incomplete answer ends with `(stopped)` or `(failed: …)`.
Each input receives one reply.
After `follow`, the script also receives one reply for every other turn in its checkout, without duplicates.

```python
#!/usr/bin/env python3
# ---
# description = "Watch a URL and report each content change. Run in the background until stopped."
# background = true
# [required]
# url = "The page to watch."
# ---
import hashlib, json, os, sys, time, urllib.request

if os.environ.get("PI_EVENTS_FD") != "3":
    sys.exit("needs pi to keep it running")
url = json.load(sys.stdin)["url"]
fd3 = os.fdopen(3, "w")
def say(**m):
    fd3.write(json.dumps(m) + "\n"); fd3.flush()

say(status=f"watching {url}; each change arrives as a turn")
seen = None
while True:
    digest = hashlib.sha256(urllib.request.urlopen(url).read()).hexdigest()
    if seen and digest != seen:
        say(result=f"{url} changed")
    seen = digest
    say(status=f"checked {time.strftime('%H:%M')}")
    time.sleep(300)
```

### Background lifetime and channels

A background wrapper returns a command id immediately, before slow work starts.
The command runs until it exits, is stopped, or pi exits. There is no timeout.
Stopping sends SIGTERM, then SIGKILL, to its process group.
Use bash controls to read output, stop, or restart the command.
Restart reuses the original JSON input and reloads the tool directory’s `.env`.

The events `result`, `input`, `interrupt`, and `follow` require background mode.
At exit, stdout becomes the command's final result. A nonzero exit also includes stderr.
In background mode, `status` and stderr are for the user during execution.
Use `status` for what the user needs to know or do now, such as scanning a QR code.
Use stderr for supporting content, such as the QR code itself.
Only a failing exit sends stderr to the model.

The script manages its own persistent state under `$PI_HOME`.
Restrict saved login tokens to owner access.
Use `input` only for words a person typed to this session.
Send data discovered by the command as `result`.

One-shot invocations and subagents do not offer background script wrappers.
Foreground scripts can still use `status` and `choose`.

To check the event protocol manually, redirect fd 3 to the terminal:
`echo '{"url":"https://example.com"}' | PI_EVENTS_FD=3 python3 <tools-dir>/watch 3>&1`.
