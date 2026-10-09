use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tool::{Concurrency, Ctx, Tier, Tool, ToolError, ToolOutput};

const TIMEOUT: Duration = Duration::from_secs(300);

/// Past this size an argument value rides on stdin only.
const MAX_ENV_VALUE: usize = 64 << 10;

/// Whether the name is safe to set as an environment variable.
pub(super) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One discovered script: the frontmatter is the contract the schema shows the
/// model, the file is the code a run executes.
pub struct Script {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) args: Vec<(String, String)>,
    pub(crate) path: PathBuf,
    pub(crate) runner: Runner,
}

/// How a script is started.
pub(crate) enum Runner {
    // A cargo script, built on its first run.
    Cargo,
    // The interpreter its `#!` line names, and the one argument the line may
    // add: read here rather than by the kernel, so no execute bit is needed.
    Shebang(String, Option<String>),
}

/// The interpreter a `#!` first line names, split as the kernel splits it:
/// the program, then everything after it as one argument.
pub(crate) fn shebang(text: &str) -> Option<(String, Option<String>)> {
    let line = text.lines().next()?.strip_prefix("#!")?.trim();
    let (program, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    (!program.is_empty()).then(|| {
        (
            program.to_string(),
            (!rest.is_empty()).then(|| rest.to_string()),
        )
    })
}

impl Script {
    fn command(&self) -> tokio::process::Command {
        match &self.runner {
            Runner::Cargo => cargo_script(&self.path),
            Runner::Shebang(program, arg) => {
                let mut command = tokio::process::Command::new(program);
                command.args(arg).arg(&self.path);
                command
            }
        }
    }

    // Declared string args also ride in as environment, under three guards: a
    // portable name, a value already set (inherited or `.env`) that always
    // wins, and a size cap.
    fn env<'a>(
        &'a self,
        args: &'a Value,
        set: &'a [(String, String)],
    ) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.args.iter().filter_map(move |(name, _)| {
            let value = args.get(name).and_then(Value::as_str)?;
            (is_identifier(name)
                && std::env::var_os(name).is_none()
                && set.iter().all(|(key, _)| key != name)
                && value.len() <= MAX_ENV_VALUE)
                .then_some((name.as_str(), value))
        })
    }
}

/// The command that runs a Rust script, building it first when it changed.
pub fn cargo_script(path: &Path) -> tokio::process::Command {
    // `-Zscript` is nightly-only as of cargo 1.93; once `cargo <file>.rs`
    // runs on stable, drop `+nightly` and `-Zscript`.
    let mut command = tokio::process::Command::new("cargo");
    command.args(["+nightly", "-Zscript", "--quiet"]).arg(path);
    command
}

/// Run a script once outside any tool call, in the workspace root: `input` on
/// stdin, stdout back, or the exit status and stderr when it fails.
pub async fn run_script(
    path: &Path,
    input: Vec<u8>,
    timeout: Duration,
    ctx: &Ctx,
) -> Result<String, String> {
    let mut command = cargo_script(path);
    command.current_dir(ctx.workspace.root());
    let exited = crate::process::run(command, Some(input), timeout, ctx)
        .await
        .map_err(|e| e.to_string())?;
    if !exited.status.success() {
        return Err(format!("{}\n{}", exited.status, exited.stderr.text.trim()));
    }
    Ok(exited.stdout.text)
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
        // Read per call, so an edit to it needs no restart. What the process
        // inherited wins over it, so one run can override a value.
        let shared: Vec<_> = self
            .path
            .parent()
            .map(super::env::read)
            .unwrap_or_default()
            .into_iter()
            .filter(|(key, _)| std::env::var_os(key).is_none())
            .collect();
        let mut command = self.command();
        command
            // Cargo finds a script's config from the script's directory, not
            // this one, so the workspace cannot configure the build.
            .current_dir(ctx.workspace.root())
            .envs(shared.iter().map(|(k, v)| (k, v)))
            .envs(self.env(&args, &shared));
        let input = serde_json::to_vec(&args).unwrap_or_default();
        let exited = crate::process::run(command, Some(input), TIMEOUT, ctx).await?;
        let (status, errs) = (exited.status, exited.stderr);

        if !status.success() {
            let mut message = format!("{}: exited {status}; {}", self.name, errs.text.trim());
            if let Some(spilled) = &errs.spill {
                message.push_str(&format!("{}\n", spilled.note()));
            }
            return Err(ToolError::Invalid(message));
        }

        let captured = exited.stdout;
        if captured.text.trim().is_empty() {
            return Ok(ToolOutput::text(format!("{}: no output", self.name)));
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

    // PATH and other inherited names are never shadowed by a call's argument:
    // otherwise a script would be hijackable by its own caller.
    #[test]
    fn an_inherited_name_is_never_shadowed() {
        let script = Script {
            name: "spy".into(),
            description: "Report PATH".into(),
            args: vec![
                ("PATH".into(), "ignored, inherited wins".into()),
                ("file".into(), "what to read".into()),
                ("bad-name".into(), "not portable".into()),
            ],
            path: PathBuf::from("spy.rs"),
            runner: Runner::Cargo,
        };
        let args = json!({
            "PATH": "/hijacked",
            "file": "a.txt",
            "bad-name": "x",
        });
        let env: Vec<_> = script.env(&args, &[]).collect();
        assert_eq!(env, vec![("file", "a.txt")]);
        // Nor is a name the `.env` file sets: the model cannot swap a key.
        let set = [("file".to_string(), "from .env".to_string())];
        assert_eq!(script.env(&args, &set).count(), 0);
    }

    #[test]
    fn a_shebang_splits_as_the_kernel_splits_it() {
        assert_eq!(
            shebang("#!/usr/bin/env python3\n"),
            Some(("/usr/bin/env".into(), Some("python3".into())))
        );
        assert_eq!(
            shebang("#!/usr/bin/env -S deno run -A\n"),
            Some(("/usr/bin/env".into(), Some("-S deno run -A".into())))
        );
        assert_eq!(shebang("#!/bin/sh"), Some(("/bin/sh".into(), None)));
        assert_eq!(shebang("echo hi\n"), None);
        assert_eq!(shebang("#!\n"), None);
    }
}
