# Interface and execution

Each script must have a header and a description.
Use `description` for an interpreted script, or `[package] description` for Rust.
A missing or invalid header, description, or name produces an unusable tool entry that explains the problem.
After you fix the problem, the tool becomes available on your next turn.

The description is the caller's only tool documentation.
State what the tool does and how it formats its answer.

Each `name = "description"` under `[required]` or `[optional]` declares one string argument.
For Rust, use `[package.metadata.pi.required]` and `[package.metadata.pi.optional]`.
You can omit either table.
Omit both for a tool with no arguments.

A name cannot appear in both tables.
The table determines whether the caller must supply an argument.
Its description does not determine this requirement.

For example, declare a reminder's arguments as:

```toml
[required]
prompt = "What to do when the reminder comes due."

[optional]
after = "A delay: 30s / 10m / 2h / 1d."
every = "A period of at least 1m."
```

The caller must supply each required argument.
The script checks each value, including empty strings.
The script supplies defaults for omitted optional arguments.
An omitted argument differs from an empty string.

The script always receives the full call as JSON on stdin.
Each argument also becomes an environment variable of the same name, except when:

- The name is not a valid environment variable identifier.
- An inherited variable or `.env` already defines that name.
- The value exceeds 64 KiB.

Read stdin when any exception can apply.
An argument cannot replace an inherited variable such as `PATH`.

Put shared keys and other shared values in `.env` beside the scripts.
Use one `NAME=value` assignment per line.
Scripts receive these values as environment variables.
Ordinary `bash` calls do not receive them.
Background wrappers load them only for their subprocess.

Never put a secret in a script.
Read it from the environment.

Write the answer to stdout.
Exit with a nonzero status to report failure.
On failure, the caller receives stderr as the error.
On success, pi excludes stderr from the model result.
Background output still retains stderr for the user.

Scripts run in the workspace root at the exec tier, one call at a time.
A foreground script call has a 300-second limit.
Background wrappers have no timeout.
pi truncates large output and saves the full output to a file, as with bash.

pi reads the `#!` line itself.
The file needs no execute permission.
For Rust scripts, put crates under `[dependencies]` in the frontmatter.
