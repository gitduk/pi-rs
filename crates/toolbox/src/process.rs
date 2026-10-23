//! A child run under the workspace's clamps: its own process group, both pipes
//! in bounded captures, and a timeout and cancellation that take the tree.

use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWriteExt as _, ReadBuf};
use tokio::process::Command;
use tool::output::{Capture, Captured};
use tool::{Ctx, ToolError};

use crate::background::Peek;

/// The longest any child may run, whatever its caller asked for.
pub(crate) const MAX_RUN: Duration = tool::limit::MAX_CALL;

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
    // Its own group, so a timeout takes the whole tree. Killing the child
    // alone leaves everything it started running.
    #[cfg(unix)]
    detach(&mut cmd);

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
            // A pipe nobody reads any more would hold the child, and this wait, forever.
            if drained.is_err() {
                reap(group).await;
            }
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

/// A session of its own: its own group, and no terminal to open, so a
/// `/dev/tty` reader fails at once instead of stopping on SIGTTOU.
#[cfg(unix)]
pub(crate) fn detach(cmd: &mut Command) {
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
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

// How long a job's pipes may stay open after its process exits: past it,
// what the process left running is taken down so the job can end.
const LINGER: Duration = Duration::from_secs(2);

/// A job's process group, killed when the job goes: stopped, removed with
/// its checkout, or pi gone.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct Group(pub(crate) u32);

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.0 > 1 {
            // SAFETY: a signal to a group this job made; a gone one is ESRCH.
            unsafe { libc::killpg(self.0 as libc::pid_t, libc::SIGKILL) };
        }
    }
}

// How much of what a job printed last can be read while it runs.
const PEEK: usize = 8 * 1024;

/// The last of what a job's streams printed, both in the order it arrived.
#[derive(Clone, Default)]
struct Tail(Arc<Mutex<(Vec<u8>, usize)>>);

impl Tail {
    fn push(&self, chunk: &[u8]) {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.1 += chunk.len();
        held.0.extend_from_slice(chunk);
        let over = held.0.len().saturating_sub(PEEK);
        held.0.drain(..over);
    }

    /// What it holds, cut to whole lines once earlier bytes have gone.
    fn read(&self) -> String {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (kept, total) = (&held.0, held.1);
        if kept.len() == total {
            return String::from_utf8_lossy(kept).into_owned();
        }
        let from = kept.iter().position(|b| *b == b'\n').map_or(0, |at| at + 1);
        let shown = &kept[from..];
        format!(
            "… the first {} of {total} bytes not shown\n{}",
            total - shown.len(),
            String::from_utf8_lossy(shown)
        )
    }

    /// `pipe`, with what is read from it also kept here.
    fn tee<R>(&self, pipe: R) -> Tee<R> {
        Tee {
            pipe,
            tail: self.clone(),
        }
    }
}

struct Tee<R> {
    pipe: R,
    tail: Tail,
}

impl<R: AsyncRead + Unpin> AsyncRead for Tee<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.pipe).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = polled {
            self.tail.push(&buf.filled()[before..]);
        }
        polled
    }
}

/// Pull `pipe` into a bounded capture on a task of its own.
fn drain<R>(mut pipe: R, ctx: &Ctx) -> tokio::task::JoinHandle<Captured>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut capture = Capture::new();
        let _ = capture.drain(&mut pipe, &ctx).await;
        capture.finish()
    })
}

async fn captured(task: &mut tokio::task::JoinHandle<Captured>) -> Captured {
    task.await.unwrap_or_else(|_| Capture::new().finish())
}

/// A child's two pipes, each drained into a bounded capture on a task of its
/// own, the latest of both kept to be read while it runs: how a job is watched.
pub(crate) struct Streams {
    tail: Tail,
    stdout: tokio::task::JoinHandle<Captured>,
    stderr: tokio::task::JoinHandle<Captured>,
}

impl Streams {
    /// Take over `child`'s piped stdout and stderr.
    pub(crate) fn of(child: &mut tokio::process::Child, ctx: &Ctx) -> Self {
        let tail = Tail::default();
        let stdout = drain(tail.tee(child.stdout.take().expect("stdout is piped")), ctx);
        let stderr = drain(tail.tee(child.stderr.take().expect("stderr is piped")), ctx);
        Self {
            tail,
            stdout,
            stderr,
        }
    }

    /// The latest of what it printed, for a job to watch.
    pub(crate) fn peek(&self) -> Peek {
        let tail = self.tail.clone();
        Arc::new(move || tail.read())
    }

    /// Both captures, once both pipes have closed.
    pub(crate) async fn captured(&mut self) -> (Captured, Captured) {
        (
            captured(&mut self.stdout).await,
            captured(&mut self.stderr).await,
        )
    }

    /// Its output once its process has exited, taking down what it left
    /// holding the pipes after a moment; `None` when the job was stopped.
    pub(crate) async fn drained(
        &mut self,
        group: Option<u32>,
        stop: &tokio_util::sync::CancellationToken,
    ) -> Option<(Captured, Captured)> {
        tokio::select! {
            both = self.captured() => Some(both),
            () = tokio::time::sleep(LINGER) => {
                reap(group).await;
                Some(self.captured().await)
            }
            () = stop.cancelled() => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Past the window, the view starts on a whole line and counts what went.
    #[test]
    fn a_tail_past_its_window_starts_on_a_whole_line() {
        let tail = Tail::default();
        let line = "x".repeat(99) + "\n";
        for _ in 0..100 {
            tail.push(line.as_bytes());
        }
        let seen = tail.read();
        let (note, rest) = seen.split_once('\n').unwrap();
        assert!(rest.split_inclusive('\n').all(|l| l == line), "{seen}");
        let shown = rest.len();
        assert_eq!(
            note,
            format!("… the first {} of 10000 bytes not shown", 10000 - shown)
        );
    }
}
