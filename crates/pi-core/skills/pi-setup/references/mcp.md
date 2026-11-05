# An MCP server

Each server in `[mcp.<name>]` supplies its tools as `<name>__<tool>` after it lists them.
Names use a-z, 0-9, `-` and `_`.

If the capability already exists as an MCP server, connect that server instead of writing a tool.
Read the server's documentation for its command, URL, and required environment variables.

## Either transport

```toml
[mcp.github]                       # stdio: a program and its arguments
command = "github-mcp-server"
args    = ["stdio"]
env     = { GITHUB_TOKEN = "$GITHUB_TOKEN" }   # $NAME reads pi's environment

[mcp.docs]
url     = "https://example.com/mcp"            # Streamable HTTP
headers = { Authorization = "$DOCS_AUTHORIZATION" }  # the complete value, including "Bearer "
```

Never put a secret in the configuration file.
For an authorization header, the variable must contain the complete value, including any `Bearer ` prefix.

## Check the connection

Within a second of the save, pi starts the server.
After listing the server's tools, pi offers them as `<server>__<tool>`.
A server that cannot start appears as `<server>__unavailable`, and its description explains the failure.
Use that description and the server's documentation to diagnose the connection.
The user can list servers and their tools, or reconnect a server.
