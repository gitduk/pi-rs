---
name: pi-setup
description: Configure pi's connections — add or fix a model provider (base_url, format, api_key, catalog paths) or an MCP server (command, URL, environment). Use when a model or an MCP server is missing, unreachable, or named in a request.
---

# Configure a connection

The `<pi_home>` block specifies pi's home.
pi uses `$PI_HOME` if you set it, or `~/.pi` otherwise.

Every connection lives in `settings.toml`.
Read this skill's own copy before writing a key it does not already show:
`skill(name: "pi-setup", file: "settings.toml")`.

## Choose the configuration file

`<pi_home>/settings.toml` applies to every checkout.
A project's `.pi/settings.toml` adds keys for that checkout alone.
Add the section to the file that matches the requested scope.
Preserve unrelated settings.

## Read the page for the job

- A model provider: `skill(name: "pi-setup", file: "references/provider.md")`.
- An MCP server: `skill(name: "pi-setup", file: "references/mcp.md")`.

## Rules for both

Never put a secret in the configuration file.
Reference an environment variable with `$NAME`.
A project that changes a provider's `base_url` or `models_url` must set `api_key` for that target; nothing is inherited from the original connection.

pi reloads within a second of a save.
A file that does not parse is reported once, and the old configuration stays.
Check what pi offers after the change before reporting success.
