use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

use tool::{Concurrency, Ctx, Tier, Tool, ToolError, ToolOutput};

const TIMEOUT: Duration = Duration::from_secs(300);

/// Past this size an argument value rides on stdin only.
const MAX_ENV_VALUE: usize = 64 << 10;

/// Whether the name is safe to set as an environment variable.
fn is_identifier(name: &str) -> bool {
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
}

impl Script {
    // Declared string args ride in as environment as well, under three
    // guards: a portable name, an inherited environment that always wins, and
    // a size past which only stdin carries the value.
    fn env<'a>(&'a self, args: &'a Value) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.args.iter().filter_map(|(name, _)| {
            let value = args.get(name).and_then(Value::as_str)?;
            (is_identifier(name)
                && std::env::var_os(name).is_none()
                && value.len() <= MAX_ENV_VALUE)
                .then_some((name.as_str(), value))
        })
    }
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
        // `-Zscript` is nightly-only as of cargo 1.93; once `cargo <file>.rs`
        // runs on stable, drop `+nightly` and `-Zscript`.
        let mut command = tokio::process::Command::new("cargo");
        command
            .args(["+nightly", "-Zscript", "--quiet"])
            .arg(&self.path)
            // Cargo finds a script's config from the script's directory, not
            // this one, so the workspace cannot configure the build.
            .current_dir(ctx.workspace.root())
            .envs(self.env(&args));
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

    // Declared args ride in as environment, but a name the process already
    // owns — `PATH` among them — is never shadowed by a call's argument: an
    // inherited environment wins, or a script is hijackable by its caller.
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
        };
        let args = json!({
            "PATH": "/hijacked",
            "file": "a.txt",
            "bad-name": "x",
        });
        let env: Vec<_> = script.env(&args).collect();
        assert_eq!(env, vec![("file", "a.txt")]);
    }
}
