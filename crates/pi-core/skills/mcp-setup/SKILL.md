---
name: mcp-setup
description: Connect or fix MCP servers for pi. Use when adding an existing MCP capability or configuring a server's command, URL, or environment.
---

# Connecting an MCP server

The `<pi_home>` block specifies pi's home: `$PI_HOME` when set, or `~/.pi` otherwise.

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
