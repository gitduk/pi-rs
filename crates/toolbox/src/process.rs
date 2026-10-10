//! A child run under the workspace's clamps: its own process group, both pipes
//! in bounded captures, and a timeout and cancellation that take the tree.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tool::output::{Capture, Captured};
use tool::{Ctx, ToolError};

/// The longest any child may run, whatever its caller asked for.
pub(crate) const MAX_RUN: Duration = Duration::from_secs(600);

pub struct Exited {
    pub status: ExitStatus,
    pub stdout: Captured,
    pub stderr: Captured,
}

/// Run `cmd`, feeding it `stdin` when there is some. Past `timeout` (at most
/// [`MAX_RUN`]), or on the context's cancellation, the process group is reaped.
pub async fn run(
    mut cmd: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    ctx: &Ctx,
) -> Result<Exited, ToolError> {
    let timeout = timeout.min(MAX_RUN);
    let input = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    cmd.stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group, so a timeout takes the whole tree. Killing the
    // child alone leaves everything it started running.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = spawn(&mut cmd)?;
    let group = child.id();

    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin is piped");
        // Fed from a detached task: EOF must reach the child when the write
        // ends, not when try_join! drops its finished arms.
        drop(tokio::spawn(async move {
            let _ = pipe.write_all(&input).await;
        }));
    }
    // wait_with_output buffers all output in memory — a runaway `yes` would
    // take the process with it. Both pipes stream into bounded captures instead.
    let mut out_pipe = child.stdout.take().expect("stdout is piped");
    let mut err_pipe = child.stderr.take().expect("stderr is piped");
    let mut out = Capture::new();
    let mut err = Capture::new();

    let waited = tokio::select! {
        r = async {
            let drained =
                tokio::try_join!(out.drain(&mut out_pipe, ctx), err.drain(&mut err_pipe, ctx));
            let status = child.wait().await?;
            drained.map(|_| status)
        } => Some(r?),
        _ = tokio::time::sleep(timeout) => None,
        _ = ctx.cancel.cancelled() => {
            reap(group).await;
            return Err(ToolError::Cancelled);
        }
    };
    let Some(status) = waited else {
        reap(group).await;
        return Err(ToolError::Timeout {
            ms: timeout.as_millis() as u64,
        });
    };
    Ok(Exited {
        status,
        stdout: out.finish(),
        stderr: err.finish(),
    })
}

/// Start `cmd`. A vanished working directory fails as a bare ENOENT, which
/// reads like a missing command; say the ground went, or the model retries.
pub(crate) fn spawn(cmd: &mut Command) -> Result<tokio::process::Child, ToolError> {
    let cwd = cmd.as_std().get_current_dir().map(|d| d.to_path_buf());
    cmd.spawn().map_err(|e| match &cwd {
        Some(cwd) if !cwd.is_dir() => ToolError::Invalid(format!(
            "the working directory is gone: {}. Nothing will run here \
             until it is back or the run moves elsewhere.",
            cwd.display()
        )),
        _ => ToolError::from(e),
    })
}

// SIGTERM the group, then SIGKILL whatever ignored it. A build killed outright
// can leave a corrupt output tree, so the polite signal goes first.
#[cfg(unix)]
pub(crate) async fn reap(group: Option<u32>) {
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
pub(crate) async fn reap(_group: Option<u32>) {}
