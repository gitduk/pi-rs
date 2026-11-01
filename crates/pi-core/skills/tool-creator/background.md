# A tool that continues after its call

For a long command, use `bash` with `background`.
It runs independently of the turn and reports its exit status and output.

For a named background tool, set `background = true` in its metadata.
Use the top level of the shebang comment block or `[package.metadata.pi]` in Rust frontmatter.
The named entry uses the same bash background executor.
It passes JSON on stdin and loads the tool directory's `.env` for its subprocess.
Examples include watchers, crawlers that return batches, and bridges that relay a person's messages to this session.

Metadata declares background mode.
It does not depend on fd 3 events.
A script reports through fd 3 events.
Read `events.md` for the protocol.

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

## Background lifetime and channels

A background wrapper returns a command ID immediately, before slow work starts.
The command runs until it exits, a stop request ends it, or pi exits.
There is no timeout.
A stop request sends SIGTERM, then SIGKILL, to the command's process group.
Use bash controls to read output, stop, or restart the command.
Restart reuses the original JSON input and reloads the tool directory’s `.env`.

The events `result`, `input`, `interrupt`, and `follow` require background mode.
At exit, stdout becomes the command's final result.
A nonzero exit also includes stderr.

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
