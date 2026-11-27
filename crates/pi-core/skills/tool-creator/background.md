# A tool that continues after its call

For a long command, use `bash` with `background`.
It runs independently of the turn and reports its exit status and output.

For a named background tool, set `background = true` in its metadata.
Use the top level of the shebang comment block or `[package.metadata.pi]` in Rust frontmatter.
The named entry uses the same bash background executor.
It passes JSON on stdin and loads the `.env` beside the tool directory for its subprocess.
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
import hashlib, json, os, sys, time, urllib.request, uuid

if os.environ.get("PI_EVENTS_FD") != "3":
    sys.exit("needs pi to keep it running")
fd3 = os.fdopen(os.dup(3), "w")
replies = os.fdopen(os.dup(3), "r")
def say(frame):
    fd3.write(json.dumps(frame) + "\n"); fd3.flush()

url = json.load(sys.stdin)["url"]
say({"id":"hello", "op":"initialize", "protocol":4})
say({"op":"status", "text":f"watching {url}"})
source = "watch:" + uuid.uuid4().hex
seen = None
while True:
    digest = hashlib.sha256(urllib.request.urlopen(url).read()).hexdigest()
    if seen and digest != seen:
        message = uuid.uuid4().hex
        say({"id":message, "op":"say", "text":f"{url} changed",
             "as":"task", "source":source, "reason":"content changed"})
        # Drain the receipt before producing another occurrence.
        while True:
            line = replies.readline()
            if not line:
                sys.exit("host frame connection closed")
            reply = json.loads(line)
            if reply.get("id") == message:
                if "error" in reply:
                    sys.exit(reply["error"]["message"])
                break
    seen = digest
```

## A service: one process for every session

`service = true` in the same metadata makes the tool a service. pi starts one
per machine — whichever pi boots first holds the lock, and the next one sees
it taken — so it is not a tool the model may call. What the service is given:

| Variable | What it is |
|---|---|
| `PI_SERVICE_LOCK_FD` | the fd holding the service's lock; keep it open, and exit when your work is done |
| `PI_SESSIONS_DIR` | the directory holding one socket per running session, `<session>.sock` |

Connect to every socket and speak the fd 3 frames from `events.md` on each;
`status` reaches that session's statusline `services` field. A crashed service
is restarted by any pi's next look, and one whose sockets are all gone should
exit on its own.

## Background lifetime and channels

A background wrapper returns a command ID immediately, before slow work starts.
The command runs until it exits, a stop request ends it, or pi exits.
There is no timeout.
A stop request sends SIGTERM, then SIGKILL, to the command's process group.
Use bash controls to read output, stop, or restart the command.
Restart reuses the original JSON input and reloads the `.env` beside the tool directory.

Model `message`, `observe`, and runtime controls require background mode.
Without `serve`, stdout at exit becomes the command's final automatic input.
A producer that submits its own messages declares `serve`; its exit is status only.
A nonzero exit also includes stderr.

In background mode, `status` and stderr are for the user during execution.
Use `status` for what the user needs to know or do now, such as scanning a QR code.
Use stderr for supporting content, such as the QR code itself.
Only a failing exit sends stderr to the model.

The script manages its own persistent state under `$PI_HOME`.
Restrict saved login tokens to owner access.
Use `ToModel(user)` for words a person typed to this session.
Use `ToModel(automatic)` with a stable source and event for discovered data.

One-shot invocations and subagents do not offer background script wrappers.
Foreground scripts can use runtime `status` and internal `prompt`.

To check the event protocol manually, redirect fd 3 to the terminal:
`echo '{"url":"https://example.com"}' | PI_EVENTS_FD=3 python3 <tools-dir>/watch 3>&1`.
