//! A script call that may outlive its turn. Fd 3 is a socket the script
//! writes JSON lines to: `{"status": ...}` says how far it has got;
//! `{"detach": ...}` ends the call with that text and runs on as a job;
//! after it, each `{"result": ...}` comes back as a turn of its own, and
//! so does stdout at exit. A script that never writes fd 3 runs as before.

use std::os::fd::AsRawFd;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tool::output::{Capture, Captured};
use tool::{Ctx, JobHandle, ToolError};

use crate::process::{Exited, MAX_RUN, reap, spawn};

/// The variable naming the descriptor, so a script can tell pi is listening.
pub const FD_VAR: &str = "PI_EVENTS_FD";
const FD: i32 = 3;
// The longest line fd 3 is read for; a longer one is dropped whole.
const MAX_LINE: usize = 1 << 20;

/// How a call ended: the script exited within its turn, or went on as job `id`.
pub enum Ran {
    Exited(Exited),
    Detached { said: String, id: u64 },
}

enum Said {
    Status(String),
    Detach(String),
    Result(String),
}

/// Run `cmd` with `input` on stdin and fd 3 open for what it says. Until it
/// detaches, a timeout or the turn's cancellation take the whole group.
pub async fn run(
    name: &str,
    mut cmd: Command,
    input: Vec<u8>,
    timeout: Duration,
    ctx: &Ctx,
) -> Result<Ran, ToolError> {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
    let fd = theirs.as_raw_fd();
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            // dup2 clears close-on-exec on the copy; a pair already at 3 has
            // to have it cleared by hand.
            let ok = if fd == FD {
                libc::fcntl(FD, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, FD)
            };
            if ok < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.env(FD_VAR, FD.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = spawn(&mut cmd)?;
    // Ours alone from here, or its end never reads EOF.
    drop(theirs);
    let group = child.id();

    let mut stdin = child.stdin.take().expect("stdin is piped");
    // Fed from a detached task: EOF must reach the child when the write
    // ends, whatever this call is waiting on meanwhile.
    drop(tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
    }));
    let mut stdout = drain(child.stdout.take().expect("stdout is piped"), ctx);
    let mut stderr = drain(child.stderr.take().expect("stderr is piped"), ctx);

    ours.set_nonblocking(true)?;
    let (read, _write) = tokio::net::UnixStream::from_std(ours)?.into_split();
    let mut fd3 = Fd3 {
        reader: BufReader::new(read),
        line: Vec::new(),
        skipping: false,
        open: true,
    };

    let limit = timeout.min(MAX_RUN);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            // What was said before an exit is read before the exit is seen.
            biased;
            said = fd3.next() => match said {
                Said::Status(text) => ctx.progress(text),
                Said::Detach(text) => {
                        let Some(sink) = ctx.jobs() else {
                            reap(group).await;
                            return Err(ToolError::Invalid(format!(
                                "{name} asked to go on in the background, which this run \
                                 cannot keep: nothing here would bring its results back"
                            )));
                        };
                        let stop = CancellationToken::new();
                        let job = sink.start(
                            ctx.workspace.root().to_path_buf(),
                            name.to_string(),
                            stop.clone(),
                        );
                        let id = job.id();
                        tokio::spawn(go_on(name.to_string(), child, fd3, stdout, stderr, job, stop));
                        return Ok(Ran::Detached { said: text, id });
                    }
                Said::Result(_) => tracing::warn!(
                    target: "pi::scripts", script = name, "result before detach, dropped"
                ),
            },
            // The pipes too: what the script left running may hold them, and
            // the deadline and esc still have to reach that.
            (status, stdout, stderr) = async {
                (child.wait().await, captured(&mut stdout).await, captured(&mut stderr).await)
            } => {
                return Ok(Ran::Exited(Exited { status: status?, stdout, stderr }));
            }
            () = &mut deadline => {
                reap(group).await;
                return Err(ToolError::Timeout { ms: limit.as_millis() as u64 });
            }
            () = ctx.cancel.cancelled() => {
                reap(group).await;
                return Err(ToolError::Cancelled);
            }
        }
    }
}

// A detached script, from its detach to its exit or its stop.
async fn go_on(
    name: String,
    mut child: Child,
    mut fd3: Fd3,
    mut stdout: JoinHandle<Captured>,
    mut stderr: JoinHandle<Captured>,
    job: std::sync::Arc<dyn JobHandle>,
    stop: CancellationToken,
) {
    let _group = child.id().map(Group);
    let status: std::io::Result<ExitStatus> = loop {
        tokio::select! {
            biased;
            said = fd3.next() => match said {
                Said::Status(text) => job.status(text),
                Said::Result(text) => job.result(text),
                Said::Detach(_) => {}
            },
            status = child.wait() => break status,
            // Stopped: the table already forgot it, so nothing more is said.
            () = stop.cancelled() => {
                reap(child.id()).await;
                return;
            }
        }
    };
    let (out, err) = tokio::select! {
        both = async { (captured(&mut stdout).await, captured(&mut stderr).await) } => both,
        () = stop.cancelled() => return,
    };
    if !out.text.trim().is_empty() {
        job.result(out.noted());
    }
    match status {
        Ok(status) if !status.success() => {
            job.result(format!("{name} exited {status}; {}", err.noted().trim()));
        }
        Err(e) => job.result(format!("{name} could not be waited on: {e}")),
        Ok(_) => {}
    }
    job.end();
}

fn drain<R>(mut pipe: R, ctx: &Ctx) -> JoinHandle<Captured>
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

async fn captured(task: &mut JoinHandle<Captured>) -> Captured {
    task.await.unwrap_or_else(|_| Capture::new().finish())
}

// What the script says on fd 3, a line at a time. Once it is closed it says
// nothing more, so a `select!` arm on it simply never fires again.
struct Fd3 {
    reader: BufReader<OwnedReadHalf>,
    // The line so far, kept here so a read cut short by `select!` loses nothing.
    line: Vec<u8>,
    // Inside a line past `MAX_LINE`, dropping it to its newline.
    skipping: bool,
    open: bool,
}

impl Fd3 {
    async fn next(&mut self) -> Said {
        while self.open {
            let room = (MAX_LINE + 1 - self.line.len()) as u64;
            let read = (&mut self.reader)
                .take(room)
                .read_until(b'\n', &mut self.line)
                .await;
            let ended = matches!(read, Ok(0) | Err(_));
            if ended {
                self.open = false;
            } else if self.line.last() != Some(&b'\n') && self.line.len() > MAX_LINE {
                self.line.clear();
                self.skipping = true;
                continue;
            } else if self.line.last() != Some(&b'\n') {
                // The last line, cut by the close: still said, once read whole.
                continue;
            }
            let line = std::mem::take(&mut self.line);
            if std::mem::take(&mut self.skipping) {
                tracing::warn!(target: "pi::scripts", "a line on fd 3 past {MAX_LINE} bytes, dropped");
                continue;
            }
            if let Some(said) = std::str::from_utf8(&line).ok().and_then(parse) {
                return said;
            }
        }
        std::future::pending().await
    }
}

// One line from fd 3; anything else on it is ignored, not fatal.
fn parse(line: &str) -> Option<Said> {
    let v: Value = serde_json::from_str(line).ok()?;
    let text = |key| v.get(key).and_then(Value::as_str).map(str::to_string);
    text("detach")
        .map(Said::Detach)
        .or_else(|| text("result").map(Said::Result))
        .or_else(|| text("status").map(Said::Status))
}

// A detached script's process group, killed when its job goes: stopped,
// removed with its checkout, or pi gone.
struct Group(u32);

impl Drop for Group {
    fn drop(&mut self) {
        if self.0 > 1 {
            // SAFETY: a signal to a group this job made; a gone one is ESRCH.
            unsafe { libc::killpg(self.0 as libc::pid_t, libc::SIGKILL) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    // A host that records what its one job said, and wakes a test at the end.
    #[derive(Default)]
    struct Sink {
        said: Arc<Mutex<Vec<String>>>,
        ended: Arc<Notify>,
        stop: Mutex<Option<CancellationToken>>,
    }

    struct Handle {
        said: Arc<Mutex<Vec<String>>>,
        ended: Arc<Notify>,
    }

    impl tool::JobSink for Sink {
        fn start(
            &self,
            _root: std::path::PathBuf,
            description: String,
            stop: CancellationToken,
        ) -> Arc<dyn JobHandle> {
            self.said
                .lock()
                .unwrap()
                .push(format!("start {description}"));
            *self.stop.lock().unwrap() = Some(stop);
            Arc::new(Handle {
                said: self.said.clone(),
                ended: self.ended.clone(),
            })
        }
    }

    impl JobHandle for Handle {
        fn id(&self) -> u64 {
            7
        }
        fn status(&self, text: String) {
            self.said.lock().unwrap().push(format!("status {text}"));
        }
        fn result(&self, text: String) {
            self.said
                .lock()
                .unwrap()
                .push(format!("result {}", text.trim()));
        }
        fn end(&self) {
            self.said.lock().unwrap().push("end".into());
            self.ended.notify_one();
        }
    }

    fn script(dir: &std::path::Path, body: &str) -> Command {
        let path = dir.join("script");
        std::fs::write(&path, body).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg(path).current_dir(dir);
        cmd
    }

    fn ctx(dir: &std::path::Path) -> Ctx {
        Ctx::new(tool::Workspace::new(dir).unwrap())
    }

    const SECOND: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn a_script_that_never_writes_fd_3_runs_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(dir.path(), "cat; echo \" and $PI_EVENTS_FD\"");
        let ran = run("plain", cmd, b"in".to_vec(), 5 * SECOND, &ctx(dir.path()))
            .await
            .unwrap();
        let Ran::Exited(exited) = ran else {
            panic!("it never detached")
        };
        assert_eq!(exited.stdout.text.trim(), "in and 3");
    }

    // Status before the detach goes to the call's progress; after it, every
    // line and the stdout at exit go to the job, in the order they were said.
    #[tokio::test]
    async fn a_detached_script_reports_to_its_job_until_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let progress = Arc::new(Mutex::new(Vec::new()));
        let seen = progress.clone();
        let ctx = ctx(dir.path())
            .with_jobs(sink.clone())
            .with_progress(Arc::new(move |t| seen.lock().unwrap().push(t)));
        let cmd = script(
            dir.path(),
            r#"echo '{"status":"warming up"}' >&3
echo '{"detach":"crawling 2 pages"}' >&3
echo '{"status":"1/2"}' >&3
echo '{"result":"page one"}' >&3
echo '{"result":"page two"}' >&3
echo 'all done'"#,
        );
        let ended = sink.ended.notified();
        let ran = run("crawl", cmd, Vec::new(), 5 * SECOND, &ctx)
            .await
            .unwrap();
        let Ran::Detached { said, id } = ran else {
            panic!("it detached")
        };
        assert_eq!((said.as_str(), id), ("crawling 2 pages", 7));
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("the job ended");
        assert_eq!(*progress.lock().unwrap(), ["warming up"]);
        assert_eq!(
            *sink.said.lock().unwrap(),
            [
                "start crawl",
                "status 1/2",
                "result page one",
                "result page two",
                "result all done",
                "end"
            ]
        );
    }

    #[tokio::test]
    async fn a_detached_script_that_fails_says_how() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        let cmd = script(
            dir.path(),
            "echo '{\"detach\":\"off\"}' >&3; echo broke >&2; exit 4",
        );
        let ended = sink.ended.notified();
        run("job", cmd, Vec::new(), 5 * SECOND, &ctx).await.unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("the job ended");
        let said = sink.said.lock().unwrap();
        assert!(
            said.iter()
                .any(|s| s.contains("exit status: 4") && s.contains("broke")),
            "{said:?}"
        );
    }

    // A run with nowhere to keep a job ends the script rather than leave it
    // running unlisted.
    #[tokio::test]
    async fn detaching_where_no_job_can_be_kept_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(
            dir.path(),
            "echo $$ > pid; echo '{\"detach\":\"x\"}' >&3; sleep 30",
        );
        let err = run("job", cmd, Vec::new(), 5 * SECOND, &ctx(dir.path()))
            .await
            .err()
            .expect("refused");
        assert!(err.to_string().contains("cannot keep"), "{err}");
        assert_gone(&dir.path().join("pid")).await;
    }

    #[tokio::test]
    async fn stopping_a_job_kills_its_script() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        let cmd = script(
            dir.path(),
            "echo $$ > pid; echo '{\"detach\":\"x\"}' >&3; sleep 30; echo '{\"result\":\"late\"}' >&3",
        );
        run("job", cmd, Vec::new(), 5 * SECOND, &ctx).await.unwrap();
        sink.stop.lock().unwrap().take().expect("started").cancel();
        assert_gone(&dir.path().join("pid")).await;
        assert_eq!(
            *sink.said.lock().unwrap(),
            ["start job"],
            "nothing after the stop"
        );
    }

    // What a script leaves behind holding stdout must not hold the turn past
    // its deadline.
    #[tokio::test]
    async fn a_left_behind_child_holding_stdout_still_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(dir.path(), "sleep 30 &\necho $! > pid");
        let err = run("leaky", cmd, Vec::new(), SECOND, &ctx(dir.path()))
            .await
            .err()
            .expect("timed out");
        assert!(matches!(err, ToolError::Timeout { .. }), "{err}");
        assert_gone(&dir.path().join("pid")).await;
    }

    // A line past the cap is dropped whole; what follows it is still read.
    #[tokio::test]
    async fn an_overlong_line_on_fd_3_is_dropped_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let seen = progress.clone();
        let ctx = ctx(dir.path()).with_progress(Arc::new(move |t| seen.lock().unwrap().push(t)));
        let cmd = script(
            dir.path(),
            &format!(
                "head -c {} /dev/zero | tr '\\0' x >&3; echo >&3; echo '{{\"status\":\"after\"}}' >&3",
                MAX_LINE * 3
            ),
        );
        run("big", cmd, Vec::new(), 5 * SECOND, &ctx).await.unwrap();
        assert_eq!(*progress.lock().unwrap(), ["after"]);
    }

    async fn assert_gone(pid_file: &std::path::Path) {
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..50 {
            // SAFETY: signal 0 only asks whether the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{pid} still runs");
    }
}
