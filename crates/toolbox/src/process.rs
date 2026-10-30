//! A child run under the workspace's clamps: its own process group, both pipes
//! in bounded captures, and a timeout and cancellation that take the tree.

use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWriteExt as _, ReadBuf};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tool::output::{Capture, Captured};
use tool::{Ctx, ToolError};

use crate::background::Peek;

/// The longest a bounded process call may run, whatever its caller asked for.
pub(crate) const MAX_RUN: Duration = tool::limit::MAX_CALL;

pub struct Exited {
    pub status: ExitStatus,
    pub stdout: Captured,
    pub stderr: Captured,
}

/// A child already running, its pipes already draining. Spawning and waiting
/// are apart because a background command hands this out before it ends:
/// whoever holds it says, in [`Ends`], what finishes it besides its own exit.
pub(crate) struct Launched {
    pub(crate) child: tokio::process::Child,
    pub(crate) group: Option<u32>,
    pub(crate) streams: Streams,
    pub(crate) guard: Option<Group>,
}

/// What ends a launched command short of its own exit. Every command answers
/// to a stop; only some are given a deadline, and that is the caller's word,
/// not a property of how it was started.
pub(crate) struct Ends {
    pub(crate) stop: CancellationToken,
    /// At most [`MAX_RUN`]. `None` leaves the command to run until it exits
    /// or `stop` fires.
    pub(crate) deadline: Option<Duration>,
}

/// Start `cmd` under the workspace's clamps, feeding it `stdin` when there is
/// some: its own process group, so taking it down takes the whole tree, and
/// both pipes draining into bounded captures.
///
/// wait_with_output buffers all output in memory — a runaway `yes` would take
/// the process with it — so the captures are bounded and spill instead.
pub(crate) fn launch(
    mut cmd: Command,
    stdin: Option<Vec<u8>>,
    ctx: &Ctx,
) -> Result<Launched, ToolError> {
    let input = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    cmd.stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own group, so a deadline takes the whole tree. Killing the child
    // alone leaves everything it started running.
    #[cfg(unix)]
    detach(&mut cmd);

    let mut child = spawn(&mut cmd)?;
    let group = child.id();
    let guard = group.map(Group);

    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin is piped");
        // Fed from a detached task: EOF must reach the child when the write
        // ends, not when this call does.
        drop(tokio::spawn(async move {
            let _ = pipe.write_all(&input).await;
        }));
    }
    // Pipe readers can outlive the call as an execution. Keep only what output
    // capture needs, rather than holding the turn's locks and progress.
    let streams = Streams::of(&mut child, &spills_only(ctx));
    Ok(Launched {
        child,
        group,
        streams,
        guard,
    })
}

/// Wait for `launched` to exit, or take it down when `ends` says to. The one
/// place a command's end is decided, so a deadline is something a caller asked
/// for rather than a difference between two ways of running the same thing.
pub(crate) async fn settle(launched: Launched, ends: Ends) -> Result<Exited, ToolError> {
    let Launched {
        mut child,
        group,
        mut streams,
        guard: _guard,
    } = launched;
    let deadline = ends.deadline.map(|d| d.min(MAX_RUN));
    let timed_out = || ToolError::Timeout {
        ms: deadline
            .expect("only the deadline arm reports a timeout")
            .as_millis() as u64,
    };
    // One future across both phases: the deadline is on the command, not on
    // each thing waited for in turn.
    let over = past(deadline);
    tokio::pin!(over);

    let status = tokio::select! {
        status = child.wait() => status?,
        () = ends.stop.cancelled() => {
            put_down(&mut child, group).await;
            return Err(ToolError::Cancelled);
        }
        () = &mut over => {
            put_down(&mut child, group).await;
            return Err(timed_out());
        }
    };
    let (stdout, stderr) = tokio::select! {
        both = streams.checked(group) => both?,
        () = ends.stop.cancelled() => {
            reap(group).await;
            return Err(ToolError::Cancelled);
        }
        () = &mut over => {
            reap(group).await;
            return Err(timed_out());
        }
    };
    Ok(Exited {
        status,
        stdout,
        stderr,
    })
}

// No deadline is a future that never fires, so every caller has the same arm.
async fn past(deadline: Option<Duration>) {
    match deadline {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// What a command keeps of the call's context: where output spills, not the
/// turn's cancellation or progress, which a background command would outlive.
pub(crate) fn spills_only(ctx: &Ctx) -> Ctx {
    let fresh = Ctx::new(ctx.workspace.clone());
    match ctx.session() {
        Some(id) => fresh.with_session(id, ctx.spill_root()),
        None => fresh.with_spill_root(ctx.spill_root()),
    }
}

/// Run `cmd` to completion, feeding it `stdin` when there is some. Past
/// `timeout` (at most [`MAX_RUN`]), or on the context's cancellation, the
/// process group is taken down. No `timeout` leaves it to the cancellation.
pub async fn run(
    cmd: Command,
    stdin: Option<Vec<u8>>,
    timeout: Option<Duration>,
    ctx: &Ctx,
) -> Result<Exited, ToolError> {
    let launched = launch(cmd, stdin, ctx)?;
    let ends = Ends {
        stop: ctx.cancel.clone(),
        deadline: timeout,
    };
    settle(launched, ends).await
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

/// Take down `child` and its group, and return once the child has exited:
/// signalled is not gone, and what it held, a port say, goes only with it.
pub(crate) async fn put_down(child: &mut tokio::process::Child, group: Option<u32>) {
    reap(group).await;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

// How long a command's pipes may stay open after its process exits: past it,
// what the process left running is taken down so the execution can end.
const LINGER: Duration = Duration::from_secs(2);

/// A command's process group, killed when the execution goes: stopped, removed with
/// its checkout, or pi gone.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct Group(pub(crate) u32);

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.0 > 1 {
            // SAFETY: a signal to a group this execution made; a gone one is ESRCH.
            unsafe { libc::killpg(self.0 as libc::pid_t, libc::SIGKILL) };
        }
    }
}

// How much of what an execution printed last can be read while it runs.
const PEEK: usize = 8 * 1024;

/// The last of what a command's streams printed, both in the order it arrived.
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
        String::from_utf8_lossy(&kept[from..]).into_owned()
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
fn drain<R>(
    mut pipe: R,
    ctx: &Ctx,
    failure: Arc<Mutex<Option<ToolError>>>,
) -> tokio::task::JoinHandle<Captured>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut capture = Capture::new();
        if let Err(why) = capture.drain(&mut pipe, &ctx).await {
            *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(why);
        }
        capture.finish()
    })
}

async fn captured(task: &mut tokio::task::JoinHandle<Captured>) -> Captured {
    task.await.unwrap_or_else(|_| Capture::new().finish())
}

/// A child's two pipes, each drained into a bounded capture on a task of its
/// own, the latest of both kept to be read while it runs: how an execution is watched.
pub(crate) struct Streams {
    tail: Tail,
    stdout: tokio::task::JoinHandle<Captured>,
    stderr: tokio::task::JoinHandle<Captured>,
    failure: Arc<Mutex<Option<ToolError>>>,
}

impl Streams {
    /// Take over `child`'s piped stdout and stderr.
    pub(crate) fn of(child: &mut tokio::process::Child, ctx: &Ctx) -> Self {
        let tail = Tail::default();
        let failure = Arc::default();
        let stdout = drain(
            tail.tee(child.stdout.take().expect("stdout is piped")),
            ctx,
            Arc::clone(&failure),
        );
        let stderr = drain(
            tail.tee(child.stderr.take().expect("stderr is piped")),
            ctx,
            Arc::clone(&failure),
        );
        Self {
            tail,
            stdout,
            stderr,
            failure,
        }
    }

    /// The latest of what it printed, for an execution to watch.
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

    async fn after_exit(&mut self, group: Option<u32>) -> (Captured, Captured) {
        // Keep the same future across the deadline: one pipe may already
        // have completed, and its join handle must not be awaited twice.
        let both = self.captured();
        tokio::pin!(both);
        tokio::select! {
            both = &mut both => both,
            () = tokio::time::sleep(LINGER) => {
                reap(group).await;
                both.await
            }
        }
    }

    /// Both captures after process exit, or why their output could not be kept.
    /// Reclaim descendants still holding the pipes past the exit grace period.
    pub(crate) async fn checked(
        &mut self,
        group: Option<u32>,
    ) -> Result<(Captured, Captured), ToolError> {
        let both = self.after_exit(group).await;
        match self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            Some(why) => Err(why),
            None => Ok(both),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Past the window, the view starts on a whole line.
    #[test]
    fn a_tail_past_its_window_starts_on_a_whole_line() {
        let tail = Tail::default();
        let line = "x".repeat(99) + "\n";
        for _ in 0..100 {
            tail.push(line.as_bytes());
        }
        let seen = tail.read();
        assert!(!seen.is_empty() && seen.len() < 10000);
        assert!(seen.split_inclusive('\n').all(|l| l == line), "{seen}");
    }

    // A child left holding the pipes must not keep the call waiting.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_ends_soon_after_its_shell_exits() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo hi");
        let started = std::time::Instant::now();
        let exited = run(cmd, None, Some(Duration::from_secs(20)), &ctx)
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(exited.status.success());
        assert_eq!(exited.stdout.text.trim(), "hi");
    }
}
