//! A script call that may outlive its turn. Fd 3 is a socket of JSON lines:
//! `status` says how far it has got; `detach` ends the call and runs on as a
//! job; after it, each `result` comes back as a turn, as does stdout at exit,
//! `input` is a person's line, `interrupt` stops the checkout's turn, and
//! `notice` is a line for the screen only.
//! A turn an input opened is told back: `started`, then its whole `reply`.

use std::os::fd::AsRawFd;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::unix::OwnedWriteHalf;
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tool::output::Captured;
use tool::{Ctx, JobHandle, Told, ToolError};

use crate::process::{Exited, Group, MAX_RUN, captured, drain, drained, reap, spawn};

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
    Input(String),
    Notice(String),
    Interrupt,
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
    let (read, write) = tokio::net::UnixStream::from_std(ours)?.into_split();
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
                        let (told, hear) = unbounded_channel();
                        let job = sink.start(
                            ctx.workspace.root().to_path_buf(),
                            name.to_string(),
                            stop.clone(),
                            Some(told),
                        );
                        tokio::spawn(tell(write, hear));
                        let id = job.id();
                        // Owned before the task is first polled: a shutdown that
                    // drops it unrun still takes what the script started.
                    let group = child.id().map(Group);
                    let name = name.to_string();
                    tokio::spawn(async move {
                        let _group = group;
                        go_on(name, child, fd3, stdout, stderr, job, stop).await;
                    });
                        return Ok(Ran::Detached { said: text, id });
                    }
                Said::Result(_) | Said::Input(_) | Said::Notice(_) | Said::Interrupt => tracing::warn!(
                    target: "pi::scripts", script = name, "said before detach, dropped"
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
    let group = child.id();
    let status: std::io::Result<ExitStatus> = loop {
        tokio::select! {
            biased;
            said = fd3.next() => match said {
                Said::Status(text) => job.status(text),
                Said::Result(text) => job.result(text, Default::default()),
                Said::Input(text) => job.input(text),
                Said::Notice(text) => job.notice(text),
                Said::Interrupt => job.interrupt(),
                Said::Detach(_) => {}
            },
            status = child.wait() => break status,
            // Stopped: the table already forgot it, so nothing more is said.
            () = stop.cancelled() => {
                reap(group).await;
                return;
            }
        }
    };
    let Some((out, err)) = drained(&mut stdout, &mut stderr, group, &stop).await else {
        return;
    };
    // One turn for one ending: what it printed, then how it failed, if it did.
    let failed = match status {
        Ok(status) if !status.success() => {
            Some(format!("{name} exited {status}; {}", err.noted().trim()))
        }
        Ok(_) => None,
        Err(e) => Some(format!("{name} could not be waited on: {e}")),
    };
    let said: Vec<String> = [Some(out.noted()), failed]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !said.is_empty() {
        job.result(said.join("\n\n"), Default::default());
    }
    job.end();
}

// What the host tells a detached script, in order, on its own task: a script
// that never reads fd 3 fills the socket and stalls only this.
async fn tell(mut to: OwnedWriteHalf, mut hear: UnboundedReceiver<Told>) {
    while let Some(told) = hear.recv().await {
        let line = match told {
            Told::Started => serde_json::json!({ "started": true }),
            Told::Reply(text) => serde_json::json!({ "reply": text }),
        };
        if to.write_all(format!("{line}\n").as_bytes()).await.is_err() {
            return;
        }
    }
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
    if v.get("interrupt").and_then(Value::as_bool) == Some(true) {
        return Some(Said::Interrupt);
    }
    text("detach")
        .map(Said::Detach)
        .or_else(|| text("result").map(Said::Result))
        .or_else(|| text("input").map(Said::Input))
        .or_else(|| text("notice").map(Said::Notice))
        .or_else(|| text("status").map(Said::Status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Sink, ctx};
    use std::sync::{Arc, Mutex};

    fn script(dir: &std::path::Path, body: &str) -> Command {
        let path = dir.join("script");
        std::fs::write(&path, body).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg(path).current_dir(dir);
        cmd
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

    // What a detached script left running cannot hold its job open past its
    // own exit: the pipes get a moment, then the group goes.
    #[tokio::test]
    async fn a_detached_scripts_leftover_child_does_not_keep_its_job_running() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        let cmd = script(
            dir.path(),
            "echo '{\"detach\":\"x\"}' >&3; sleep 30 &\necho $! > pid; echo done",
        );
        let ended = sink.ended.notified();
        run("leaky", cmd, Vec::new(), 5 * SECOND, &ctx)
            .await
            .unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("the job ended");
        assert_gone(&dir.path().join("pid")).await;
        assert!(
            sink.said
                .lock()
                .unwrap()
                .contains(&"result done".to_string())
        );
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

    // The shipped `later` is the protocol's worked example: it has to keep
    // detaching and reporting the way this side reads it.
    #[tokio::test]
    async fn the_shipped_later_detaches_then_brings_its_prompt_back() {
        let Ok(python) = which("python3") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let later =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/tools/later");
        let mut cmd = Command::new(python);
        cmd.arg(later).current_dir(dir.path());
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        let ended = sink.ended.notified();
        let input = br#"{"prompt": "check the build", "after": "0s"}"#.to_vec();
        let Ran::Detached { said, .. } = run("later", cmd, input, 5 * SECOND, &ctx).await.unwrap()
        else {
            panic!("it detached")
        };
        assert!(said.contains("back in 0s"), "{said}");
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("it ended");
        let said = sink.said.lock().unwrap();
        let result = said
            .iter()
            .find(|s| s.starts_with("result "))
            .expect("a result");
        assert!(result.contains("check the build"), "{said:?}");
    }

    fn which(program: &str) -> Result<std::path::PathBuf, ()> {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(program))
                    .find(|path| path.is_file())
            })
            .ok_or(())
    }

    // A person's line and a stop go up; what the host tells of the turn
    // comes down on the same fd, in order, for the script to read.
    #[tokio::test]
    async fn a_detached_script_speaks_for_a_person_and_hears_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        let cmd = script(
            dir.path(),
            r#"echo '{"detach":"connected"}' >&3
echo '{"input":"why did it fail"}' >&3
echo '{"interrupt":true}' >&3
read -r a <&3; read -r b <&3
printf '%s\n%s\n' "$a" "$b" > heard"#,
        );
        let ended = sink.ended.notified();
        run("bridge", cmd, Vec::new(), 5 * SECOND, &ctx)
            .await
            .unwrap();
        let told = sink.told.lock().unwrap().clone().expect("it listens");
        told.send(Told::Started).unwrap();
        told.send(Told::Reply("parse.rs:40".into())).unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("it ended");
        assert_eq!(
            sink.said.lock().unwrap()[1..3],
            ["input why did it fail", "interrupt"]
        );
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        assert_eq!(heard, "{\"started\":true}\n{\"reply\":\"parse.rs:40\"}\n");
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
