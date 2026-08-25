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

/// Write the whole of an over-long output somewhere the model can go back to.
/// Returns the path when there was anything worth keeping.
fn spill(stdout: &str, stderr: &str) -> Option<String> {
    if stdout.len() <= MAX_OUTPUT && stderr.len() <= MAX_OUTPUT {
        return None;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pi-bash-{}-{n}.log", std::process::id()));
    let whole = format!("<stdout>\n{stdout}\n</stdout>\n<stderr>\n{stderr}\n</stderr>\n");
    std::fs::write(&path, whole).ok()?;
    Some(path.display().to_string())
}

/// SIGTERM the group, then SIGKILL whatever ignored it. A build killed outright
/// can leave a corrupt output tree, so the polite signal goes first.
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

/// Windows has no process group to signal: the direct child still dies with
/// `kill_on_drop`, but its descendants outlive a timeout.
#[cfg(not(unix))]
async fn reap(_group: Option<u32>) {}

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
        let args: Args = crate::parse_args(args)?;
        let cwd = match &args.cwd {
            Some(p) => ctx.workspace.resolve(p)?,
            None => ctx.workspace.root().to_path_buf(),
        };
        // A zero would otherwise kill the command before it started.
        let timeout = std::time::Duration::from_millis(
            args.timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
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
                command = %args.command,
                timeout_ms = timeout.as_millis() as u64,
                "timed out"
            );
            reap(group).await;
            return Ok(ToolOutput::text(format!(
                "timed out after {}ms; the command and everything it spawned were killed",
                timeout.as_millis()
            )));
        };

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let code = out.status.code().unwrap_or(-1);

        // Anything elided is written out first, so a build log the model needs
        // the middle of is one grep away rather than gone.
        let spilled = spill(&stdout, &stderr);

        // The exit code and the command, always. What the command printed is
        // in the transcript; what it was run against — the directory — is not.
        tracing::info!(
            target: "pi::bash",
            command = %args.command,
            cwd = %cwd.display(),
            code,
            stdout_bytes = stdout.len(),
            stderr_bytes = stderr.len(),
            "exited"
        );

        let mut body = String::new();
        body.push_str(&clamp("stdout", stdout.trim_end()));
        body.push_str(&clamp("stderr", stderr.trim_end()));
        if let Some(path) = spilled {
            body.push_str(&format!("full output: {path}\n"));
        }
        if code != 0 {
            body.push_str(&format!("exit {code}\n"));
        }
        if body.is_empty() {
            return Ok(ToolOutput::useless("exit 0, no output"));
        }
        // A progress line says what ran and is one line: the diff-row
        // renderer and the journal treat every newline after the first as
        // structure, not text.
        let preview = args.command.split('\n').next().unwrap_or_default();
        Ok(ToolOutput::text(body).with_preview(preview))
    }
}
