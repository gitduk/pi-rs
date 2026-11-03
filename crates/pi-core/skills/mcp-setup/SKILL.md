---
name: mcp-setup
description: Connect or fix MCP servers for pi. Use when adding an existing MCP capability or configuring a server's command, URL, or environment.
---

# Connect or fix an MCP server

The `<pi_home>` block specifies pi's home.
pi uses `$PI_HOME` if you set it, or `~/.pi` otherwise.

If the capability exists as an MCP server, connect that server instead of writing a tool.
Check the server's documentation for its command, URL, and required environment variables.
Read the existing configuration before editing it.

## Choose the configuration file

`<pi_home>/settings.toml` applies to every checkout.
A project's `.pi.toml` adds servers for that checkout alone.
pi reports the project's servers at startup.
Add the server section to the file that matches the requested scope.
Preserve unrelated settings and servers.

## Configure the server

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

Never put a secret in the configuration file.
Reference an environment variable with `$NAME`.
For an authorization header, the variable must contain the complete value, including any `Bearer ` prefix.

## Check the connection

Check the tools that pi offers after the configuration change.
A server that cannot start appears as `<server>__unavailable`.
Its description explains the failure.
Use that failure description and the server's documentation to diagnose the connection.
The user can list servers and their tools, or reconnect a server.
