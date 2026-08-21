use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::process::Command;

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT: usize = 30_000;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

/// Largest prefix of `s` within `max` bytes that ends on a char boundary.
pub(crate) fn head(s: &str, max: usize) -> &str {
    match s.char_indices().find(|(i, c)| i + c.len_utf8() > max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Mirror of [`head`] from the end.
fn tail(s: &str, max: usize) -> &str {
    let start = s.len().saturating_sub(max);
    match s.char_indices().find(|(i, _)| *i >= start) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

/// Keep both ends: the head carries what the command set out to do, the tail
/// carries how it failed.
fn clamp(label: &str, body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    if body.len() <= MAX_OUTPUT {
        return format!("<{label}>\n{body}\n</{label}>\n");
    }
    let half = MAX_OUTPUT / 2;
    let (h, t) = (head(body, half), tail(body, half));
    let dropped = body.len() - h.len() - t.len();
    format!("<{label}>\n{h}\n… {dropped} bytes elided …\n{t}\n</{label}>\n")
}

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace. Each call is a fresh shell: cd and \
         environment changes do not carry over. Prefer read and write over cat \
         and heredocs."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_ms": { "type": "integer", "description": "Default 120000, max 600000." },
                "cwd": { "type": "string", "description": "Workspace-relative. Default the workspace root." },
            },
            "required": ["command"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)?;
        let cwd = match &args.cwd {
            Some(p) => ctx.workspace.resolve(p)?,
            None => ctx.workspace.root().to_path_buf(),
        };
        let timeout = std::time::Duration::from_millis(
            args.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        let child = Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let waited = tokio::select! {
            r = child.wait_with_output() => Some(r?),
            _ = tokio::time::sleep(timeout) => None,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        };

        let Some(out) = waited else {
            return Ok(ToolOutput::text(format!(
                "timed out after {}ms; the shell was killed",
                timeout.as_millis()
            )));
        };

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let code = out.status.code().unwrap_or(-1);

        let mut body = String::new();
        body.push_str(&clamp("stdout", stdout.trim_end()));
        body.push_str(&clamp("stderr", stderr.trim_end()));
        if code != 0 {
            body.push_str(&format!("exit {code}\n"));
        }
        if body.is_empty() {
            return Ok(ToolOutput::useless("exit 0, no output"));
        }
        // The first line of the result is `<stdout>`; the first line of the
        // command's own output is what a progress display should show.
        let first = stdout
            .lines()
            .chain(stderr.lines())
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output");
        let summary = format!(
            "{first}{}",
            if code == 0 {
                String::new()
            } else {
                format!(" · exit {code}")
            }
        );
        Ok(ToolOutput::text(body).with_preview(summary))
    }
}
