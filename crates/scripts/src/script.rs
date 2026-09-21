use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

use tools::output::{self, Capture};
use tools::{Concurrency, Ctx, Tier, Tool, ToolError, ToolOutput};

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
pub struct Script {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) args: Vec<(String, String)>,
    pub(crate) path: PathBuf,
}

#[async_trait]
impl Tool for Script {
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
                tools::bash::reap(group).await;
                return Err(ToolError::Cancelled);
            }
        };

        let Some((status, errs)) = waited else {
            tools::bash::reap(group).await;
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
    use crate::discover_in;
    use serde_json::json;

    // Declared args ride in as environment, but a name the process already
    // owns — `PATH` among them — is never shadowed by a call's argument: an
    // inherited environment wins, or a script is hijackable by its caller.
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
        let ctx = Ctx::new(tools::Workspace::new(dir.path()).unwrap());
        let out = found
            .remove(0)
            .execute(json!({ "PATH": "/hijacked" }), &ctx)
            .await
            .unwrap();
        assert_ne!(out.flatten(), "/hijacked");
    }
}
