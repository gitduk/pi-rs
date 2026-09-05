use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::process::Command;

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput, spill};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// The longest any command may run, whatever its caller asked for.
const MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

pub struct Bash;

// SIGTERM the group, then SIGKILL whatever ignored it. A build killed outright
// can leave a corrupt output tree, so the polite signal goes first.
#[cfg(unix)]
async fn reap(group: Option<u32>) {
    // A freshly spawned pid can never equal our own group's id, and the filter
    // rejects 0 — `killpg(0, …)` would signal the agent itself.
    let Some(pid) = group.filter(|p| *p > 1) else {
        return;
    };
    let pid = pid as i32;
    unsafe { libc::killpg(pid, libc::SIGTERM) };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    unsafe { libc::killpg(pid, libc::SIGKILL) };
}

// Windows has no process group to signal: the direct child still dies with
// `kill_on_drop`, but its descendants outlive a timeout.
#[cfg(not(unix))]
async fn reap(_group: Option<u32>) {}

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
        let args: Args = crate::parse_args(args)?;
        let cwd = match &args.cwd {
            Some(p) => ctx.workspace.resolve(p, self.tier())?,
            None => ctx.workspace.root().to_path_buf(),
        };
        // A zero would otherwise kill the command before it started.
        let timeout = std::time::Duration::from_millis(
            args.timeout_ms.filter(|ms| *ms > 0).unwrap_or(DEFAULT_TIMEOUT_MS),
        );

        let ran = run(&args.command, &cwd, timeout, ctx).await?;
        let mut body = ran.body;
        if ran.code != 0 {
            body.push_str(&format!("exit {}\n", ran.code));
        }
        // A progress line says what ran and is one line: the diff-row
        // renderer and the journal treat every newline after the first as
        // structure, not text.
        let preview = args.command.split('\n').next().unwrap_or_default();
        if body.is_empty() {
            // Named here too. Without it the row falls back to the body, and a
            // command that printed nothing is the one whose row is read to
            // find out what was asked.
            return Ok(ToolOutput::useless("exit 0, no output").with_preview(preview));
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
/// SIGTERM-then-SIGKILL timeout capped at [`MAX_TIMEOUT_MS`], and the
/// context's cancellation.
///
/// Public because `task` runs a caller's check through it: a second
/// implementation would be a second set of those clamps to keep right, and the
/// one that drifts is the one nothing is watching.
pub async fn run(
    command: &str,
    cwd: &std::path::Path,
    timeout: std::time::Duration,
    ctx: &Ctx,
) -> Result<Ran, ToolError> {
    let timeout = timeout.min(std::time::Duration::from_millis(MAX_TIMEOUT_MS));

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group, so a timeout takes the whole tree. Killing the
    // shell alone leaves everything it backgrounded running.
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd.spawn()?;
    // wait_with_output consumes the child, so the group id is taken first.
    let group = child.id();

    let waited = tokio::select! {
        r = child.wait_with_output() => Some(r?),
        _ = tokio::time::sleep(timeout) => None,
        _ = ctx.cancel.cancelled() => {
            reap(group).await;
            return Err(ToolError::Cancelled);
        }
    };

    let Some(out) = waited else {
        tracing::warn!(
            target: "pi::bash",
            command = %command,
            timeout_ms = timeout.as_millis() as u64,
            "timed out"
        );
        reap(group).await;
        return Err(ToolError::Timeout {
            ms: timeout.as_millis() as u64,
        });
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().unwrap_or(-1);

    // Anything omitted is written out first, so a build log the model needs
    // the middle of is one grep away rather than gone. The combined whole
    // is only assembled once either stream is known to be over the
    // threshold — a normal output must not pay for a full copy it drops.
    let spilled = if stdout.len() > spill::MAX_OUTPUT || stderr.len() > spill::MAX_OUTPUT {
        let whole = format!("<stdout>\n{stdout}\n</stdout>\n<stderr>\n{stderr}\n</stderr>\n");
        spill::write(ctx, &whole)?
    } else {
        None
    };

    // The exit code and the command, always. What the command printed is
    // in the transcript; what it was run against — the directory — is not.
    tracing::info!(
        target: "pi::bash",
        command = %command,
        cwd = %cwd.display(),
        code,
        stdout_bytes = stdout.len(),
        stderr_bytes = stderr.len(),
        "exited"
    );

    let mut body = String::new();
    body.push_str(&spill::clamp("stdout", stdout.trim_end()));
    body.push_str(&spill::clamp("stderr", stderr.trim_end()));
    if let Some(s) = spilled {
        body.push_str(&format!("{}\n", s.note()));
    }
    // The note travels with the output; the exit line does not — `code` says
    // that, and a caller that renders it as well would say it twice.
    if code != 0 && git_lock(&stderr) {
        // Two lanes committing at once collide on shared `.git/*.lock`;
        // the raw fatal reads as a broken repository, not a busy one.
        body.push_str(
            "note: git could not take a `.lock` — another lane or process is writing \
             this repository; let it finish and retry\n",
        );
    }
    Ok(Ran { code, body })
}

/// Whether a failed run tripped over git's own locking.
fn git_lock(stderr: &str) -> bool {
    (stderr.contains(".lock") && stderr.contains("fatal"))
        || stderr.contains("Another git process seems to be running")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_git_lock_failure_is_recognised() {
        let index = "fatal: Unable to create '/w/.git/index.lock': File exists.";
        let head = "fatal: cannot lock ref 'HEAD': Unable to create '/w/.git/HEAD.lock': File exists.";
        let other = "Another git process seems to be running in this repository";
        let not = "fatal: not a git repository (or any of the parent directories): .git";
        for busy in [index, head, other] {
            assert!(super::git_lock(busy), "{busy}");
        }
        assert!(!super::git_lock(not));
    }
}
