//! User-defined tools: a script dropped into `~/.pi/tools/` is a tool.
//! The comment header — from the shebang to the first non-comment line —
//! is its interface: `# description:` what it does, and every other
//! `# name: what it is` line declares one argument, however many. String
//! arguments also arrive as environment variables `$name`: absent when not
//! passed, never shadowing an inherited name, and capped in size — while
//! the call's full JSON stays on stdin and stdout is the result; stderr
//! only surfaces when the exit is non-zero. Output runs through the same
//! bounded capture as bash's, so a runaway script floods neither memory
//! nor transcript.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

use crate::output::{self, Capture};
use crate::{Concurrency, Ctx, Tier, Tool, ToolError, ToolOutput};

const TIMEOUT: Duration = Duration::from_secs(300);

/// Past this size an argument value rides on stdin only.
const MAX_ENV_VALUE: usize = 64 << 10;

/// Whether the shell can reference the name as `$name`.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One discovered script: the header is the contract the schema shows the
/// model, the file is the code a run executes.
pub struct ScriptTool {
    name: String,
    description: String,
    args: Vec<(String, String)>,
    path: PathBuf,
}

/// Scan the given directory. A script whose header carries no description
/// is not registered — a tool the model cannot see described is a trap,
/// and it is named in the skipped list instead.
pub fn discover_in(dir: &Path) -> (Vec<ScriptTool>, Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut tools = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            skipped.push(format!("{}: not utf-8", path.display()));
            continue;
        };
        let Some((description, args)) = header(&text) else {
            skipped.push(format!("{}: no # description: line", path.display()));
            continue;
        };
        tools.push(ScriptTool {
            name: name.to_string(),
            description,
            args,
            path,
        });
    }
    (tools, skipped)
}

/// The `#` comment block a script opens with is its interface declaration.
fn header(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut description = None;
    let mut args = Vec::new();
    for line in text.lines() {
        let Some(line) = line.strip_prefix('#') else {
            break;
        };
        let Some((key, rest)) = line.trim().split_once(':') else {
            continue;
        };
        let value = rest.trim();
        match key.trim() {
            "description" if !value.is_empty() => description = Some(value.to_string()),
            // Everything else in the header names an argument; the value is
            // what the schema says about it.
            key if !key.is_empty() => args.push((key.to_string(), value.to_string())),
            _ => {}
        }
    }
    description.map(|d| (d, args))
}

#[async_trait]
impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    fn schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for (name, what) in &self.args {
            properties.insert(
                name.clone(),
                json!({ "type": "string", "description": what }),
            );
            required.push(json!(name));
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let mut command = tokio::process::Command::new("bash");
        command
            .arg(&self.path)
            .current_dir(ctx.workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a timeout takes the whole tree. Killing
        // the shell alone leaves everything it backgrounded running.
        #[cfg(unix)]
        command.process_group(0);
        // Declared string args ride in as `$name` as well, under three
        // guards: a name the shell can spell, an inherited environment that
        // always wins, and a size past which only stdin carries the value.
        for (name, _) in &self.args {
            let Some(value) = args.get(name).and_then(Value::as_str) else {
                continue;
            };
            if is_identifier(name)
                && std::env::var_os(name).is_none()
                && value.len() <= MAX_ENV_VALUE
            {
                command.env(name, value);
            }
        }
        // A working directory that has gone fails as a bare ENOENT, which
        // reads exactly like a missing command — say the ground moved.
        let mut child = command.spawn().map_err(|e| {
            if ctx.workspace.root().is_dir() {
                ToolError::from(e)
            } else {
                ToolError::Invalid(format!(
                    "the working directory is gone: {}. Nothing will run \
                     here until it is back or the run moves elsewhere.",
                    ctx.workspace.root().display()
                ))
            }
        })?;
        let group = child.id();

        // Stdin is fed from a detached task: EOF must reach the script when
        // the write ends, not when try_join! drops its finished arms.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        drop(tokio::spawn(async move {
            let input = serde_json::to_vec(&args).unwrap_or_default();
            let _ = stdin.write_all(&input).await;
        }));
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let mut capture = Capture::new();

        let waited = tokio::select! {
            r = async {
                let drained =
                    tokio::try_join!(capture.drain(&mut stdout, ctx), output::take(stderr, ctx));
                let status = child.wait().await?;
                drained.map(|(_, errs)| (status, errs))
            } => Some(r?),
            _ = tokio::time::sleep(TIMEOUT) => None,
            _ = ctx.cancel.cancelled() => {
                crate::bash::reap(group).await;
                return Err(ToolError::Cancelled);
            }
        };

        let Some((status, errs)) = waited else {
            crate::bash::reap(group).await;
            return Err(ToolError::Timeout {
                ms: TIMEOUT.as_millis() as u64,
            });
        };

        if !status.success() {
            let mut message = format!("{}: exited {status}; {}", self.name, errs.text.trim());
            if let Some(spilled) = &errs.spill {
                message.push_str(&format!("{}\n", spilled.note()));
            }
            return Err(ToolError::Invalid(message));
        }

        let captured = capture.finish();
        if captured.text.trim().is_empty() {
            return Ok(ToolOutput::useless(format!("{}: no output", self.name)));
        }
        let mut body = captured.text;
        if let Some(spilled) = &captured.spill {
            body.push_str(&format!("{}\n", spilled.note()));
        }
        Ok(ToolOutput::text(body).with_preview(self.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_comment_header_is_the_declaration() {
        let text = "#!/usr/bin/env bash\n# description: Echo it back\n# msg: what to say\n# times: how often\ncat\n";
        let (description, args) = header(text).unwrap();
        assert_eq!(description, "Echo it back");
        assert_eq!(
            args,
            vec![
                ("msg".to_string(), "what to say".to_string()),
                ("times".to_string(), "how often".to_string())
            ]
        );
    }

    #[test]
    fn a_script_without_a_description_is_no_tool() {
        assert!(header("#!/bin/sh\necho hi\n").is_none());
    }

    #[tokio::test]
    async fn a_script_runs_with_the_call_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(
            tools.join("echo.sh"),
            "#!/usr/bin/env bash\n# description: Echo stdin\ncat\n",
        )
        .unwrap();
        let (mut found, skipped) = discover_in(&tools);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(found.len(), 1);
        let ctx = Ctx::new(crate::Workspace::new(dir.path()).unwrap());
        let out = found
            .remove(0)
            .execute(json!({ "x": 1 }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.flatten(), "{\"x\":1}");
    }

    #[tokio::test]
    async fn declared_string_args_ride_in_as_environment() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(
            tools.join("greet.sh"),
            "#!/usr/bin/env bash\n# description: Greet\n# name: who\n# punct: ending\nprintf '%s|%s|%s' \"$name\" \"$punct\" \"${c:-none}\"\n",
        )
        .unwrap();
        let (mut found, _) = discover_in(&tools);
        let ctx = Ctx::new(crate::Workspace::new(dir.path()).unwrap());
        let out = found
            .remove(0)
            .execute(json!({ "name": "hi", "punct": "!" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.flatten(), "hi|!|none");
    }

    #[tokio::test]
    async fn an_inherited_name_is_never_shadowed() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(
            tools.join("spy.sh"),
            "#!/usr/bin/env bash\n# description: Report PATH\n# PATH: ignored, inherited wins\nprintf '%s' \"$PATH\"\n",
        )
        .unwrap();
        let (mut found, _) = discover_in(&tools);
        let ctx = Ctx::new(crate::Workspace::new(dir.path()).unwrap());
        let out = found
            .remove(0)
            .execute(json!({ "PATH": "/hijacked" }), &ctx)
            .await
            .unwrap();
        assert_ne!(out.flatten(), "/hijacked");
    }
}
