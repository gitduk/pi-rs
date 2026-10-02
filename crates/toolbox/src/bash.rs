use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

pub struct Bash;

impl Bash {
    pub const NAME: &'static str = "bash";
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace. Each call is a fresh shell: cd and \
         environment changes do not carry over — pass cwd rather than prefixing \
         cd. Prefer read, edit and write over cat, heredocs and sed -i: they \
        report failures you can act on. File moves and deletions belong \
        here too (mv, rm); content edits belong to edit."
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
        let args: Args = tool::parse_args(args)?;
        let cwd = match &args.cwd {
            Some(p) => ctx.workspace.resolve(p, self.tier())?,
            None => ctx.workspace.root().to_path_buf(),
        };
        // A zero would otherwise kill the command before it started.
        let timeout = std::time::Duration::from_millis(
            args.timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
        );

        // rtk rewrites `command` in its own vocabulary; the rewrite is what runs.
        // Run from `cwd` for project filters; races cancellation so Esc isn't delayed.
        let rewritten = tokio::select! {
            r = crate::rtk::rewrite(&args.command, &cwd) => r,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        let command = rewritten.unwrap_or(args.command);

        let ran = run(&command, &cwd, timeout, ctx).await?;
        let mut body = ran.body;
        if ran.code != 0 {
            body.push_str(&format!("exit {}\n", ran.code));
        }
        // One line: the diff-row renderer and journal treat further newlines as
        // structure, not text.
        let preview = command.split('\n').next().unwrap_or_default();
        if body.is_empty() {
            // Preview stays set: without it the row falls back to body, hiding
            // what a silent command was asked to do.
            return Ok(ToolOutput::text("exit 0, no output").with_preview(preview));
        }
        Ok(ToolOutput::text(body).with_preview(preview))
    }
}

/// What a finished command left behind.
pub struct Ran {
    pub code: i32,
    /// What the command printed, plus a spill note for what did not fit and
    /// any note explaining the failure. Empty when it succeeded silently.
    pub body: String,
}

/// Run `command` under the workspace's clamps — its own process group, a
/// SIGTERM-then-SIGKILL timeout capped at ten minutes, and the
/// context's cancellation.
pub async fn run(
    command: &str,
    cwd: &std::path::Path,
    timeout: std::time::Duration,
    ctx: &Ctx,
) -> Result<Ran, ToolError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    let exited = crate::process::run(cmd, None, timeout, ctx)
        .await
        .inspect_err(|e| {
            if let ToolError::Timeout { ms } = e {
                tracing::warn!(target: "pi::bash", command = %command, timeout_ms = ms, "timed out");
            }
        })?;
    let code = exited.status.code().unwrap_or(-1);
    let (stdout, stderr) = (exited.stdout, exited.stderr);

    // The exit code and the command, always. What the command printed is
    // in the transcript; what it was run against — the directory — is not.
    tracing::info!(
        target: "pi::bash",
        command = %command,
        cwd = %cwd.display(),
        code,
        stdout_bytes = stdout.total,
        stderr_bytes = stderr.total,
        "exited"
    );

    let mut body = String::new();
    body.push_str(&section("stdout", &stdout));
    body.push_str(&section("stderr", &stderr));
    for captured in [&stdout, &stderr] {
        if let Some(s) = &captured.spill {
            body.push_str(&format!("{}\n", s.note()));
        }
    }
    // No exit line in the body: `code` says that, and a caller that renders
    // it as well would say it twice.
    Ok(Ran { code, body })
}

fn section(label: &str, s: &output::Captured) -> String {
    let body = s.text.trim_end();
    if body.is_empty() {
        return String::new();
    }
    format!("<{label}>\n{body}\n</{label}>\n")
}
