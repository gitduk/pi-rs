//! The duplex JSON protocol shared by scripts and bash background commands.
use crate::background::{OutputMode, RunHandle};
use crate::channel::{Answer, Ask, Asked, Told};
use crate::process::{Exited, Group, MAX_RUN, Streams, put_down, reap, spawn};
use serde_json::Value;
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use tool::{Ctx, ToolError};

pub const FD_VAR: &str = "PI_EVENTS_FD";
const FD: i32 = 3;
const MAX_LINE: usize = 1 << 20;

enum Said {
    Status(String),
    Result(String),
    Input(String),
    Choose(Asked),
    Answer { question: u64, picks: Vec<usize> },
    Interrupt,
    Follow,
}

struct Process {
    child: Child,
    streams: Streams,
    fd3: Fd3,
    told: UnboundedSender<Told>,
    guard: Option<Group>,
}

fn start(mut cmd: Command, input: Vec<u8>, ctx: &Ctx) -> Result<Process, ToolError> {
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
        .kill_on_drop(true);
    crate::process::detach(&mut cmd);
    let mut child = spawn(&mut cmd)?;
    // Ours alone from here, or its end never reads EOF.
    drop(theirs);
    let guard = child.id().map(Group);

    let mut stdin = child.stdin.take().expect("stdin is piped");
    // Fed from a detached task: EOF must reach the child when the write
    // ends, whatever this call is waiting on meanwhile.
    drop(tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
    }));
    let streams = Streams::of(&mut child, ctx);

    ours.set_nonblocking(true)?;
    let (read, write) = tokio::net::UnixStream::from_std(ours)?.into_split();
    let fd3 = Fd3 {
        reader: BufReader::new(read),
        line: Vec::new(),
        skipping: false,
        open: true,
    };
    let (told, hear) = unbounded_channel();
    tokio::spawn(tell(write, hear));

    Ok(Process {
        child,
        streams,
        fd3,
        told,
        guard,
    })
}

/// Run a foreground script with its questions attached to the calling turn.
pub async fn foreground(
    name: &str,
    cmd: Command,
    input: Vec<u8>,
    timeout: Duration,
    ctx: &Ctx,
    asks: &dyn Ask,
) -> Result<Exited, ToolError> {
    let Process {
        mut child,
        mut streams,
        mut fd3,
        told,
        guard: _guard,
    } = start(cmd, input, ctx)?;
    let group = child.id();
    let limit = timeout.min(MAX_RUN);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    // Waiting on the person is not running long: the clock stops until the
    // answer, then starts over.
    let mut waiting: Option<Answer> = None;
    loop {
        tokio::select! {
            // Stop and deadline take priority over a busy event channel.
            // Events already buffered are read before observing process exit.
            biased;
            () = ctx.cancel.cancelled() => {
                reap(group).await;
                return Err(ToolError::Cancelled);
            }
            picked = async { waiting.as_mut().expect("guarded").await }, if waiting.is_some() => {
                waiting = None;
                let _ = told.send(Told::Answer(Ok(picked.ok().flatten())));
                deadline.as_mut().reset(tokio::time::Instant::now() + limit);
            }
            () = &mut deadline, if waiting.is_none() => {
                reap(group).await;
                return Err(ToolError::Timeout {
                    ms: limit.as_millis() as u64,
                    printed: String::new(),
                });
            }
            said = fd3.next() => match said {
                Said::Status(text) => ctx.progress(text),
                Said::Choose(asked) => {
                    let root = ctx.workspace.root().to_path_buf();
                    let put = match waiting {
                        Some(_) if asked.back => {
                            Err("one question at a time: the last is not answered yet".into())
                        }
                        _ => asks.ask(root, name, asked),
                    };
                    match put {
                        Ok(Some(answer)) => waiting = Some(answer),
                        Ok(None) => {
                            let _ = told.send(Told::Asked);
                        }
                        Err(why) => {
                            let _ = told.send(Told::Answer(Err(why)));
                        }
                    }
                }
                Said::Result(_)
                | Said::Input(_)
                | Said::Answer { .. }
                | Said::Interrupt
                | Said::Follow => tracing::warn!(
                    target: "pi::scripts", script = name, "session message requires background=true, dropped"
                ),
            },
            // The pipes too: what the script left running may hold them, and
            // the deadline and a stop still have to reach that.
            (status, captures) = async {
                (child.wait().await, streams.checked(group).await)
            } => {
                let (stdout, stderr) = captures?;
                return Ok(Exited { status: status?, stdout, stderr });
            }
        }
    }
}

/// Start one already-registered command and attach its session channel.
pub(crate) fn background(
    cmd: Command,
    input: Vec<u8>,
    ctx: &Ctx,
    execution: Arc<dyn RunHandle>,
    stop: CancellationToken,
    output: OutputMode,
) -> Result<(), ToolError> {
    let Process {
        mut child,
        mut streams,
        mut fd3,
        told,
        guard,
    } = start(cmd, input, ctx)?;
    execution.watch(streams.peek());
    execution.channel().listen(told.clone());
    tokio::spawn(async move {
        let _guard = guard;
        let group = child.id();
        let status = loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => { put_down(&mut child, group).await; return; }
                event = fd3.next() => match event {
                    Said::Status(text) => execution.channel().status(text),
                    Said::Result(text) => execution.channel().result(text),
                    Said::Input(text) => execution.channel().input(text),
                    Said::Choose(asked) if asked.back => {
                        let _ = told.send(Told::Answer(Err("a background command cannot wait for a foreground answer".into())));
                    }
                    Said::Choose(asked) => execution.channel().choose(asked),
                    Said::Answer {question,picks} => execution.channel().answer(question,picks),
                    Said::Interrupt => execution.channel().interrupt(),
                    Said::Follow => execution.channel().follow(),
                },
                status = child.wait() => break status,
            }
        };
        let captures = tokio::select! {
            biased;
            // The child is gone, but what it started may still be draining
            // the pipes — and holding what a restart needs.
            () = stop.cancelled() => { reap(group).await; return; }
            captures = streams.checked(group) => captures,
        };
        let result = match (status, captures) {
            (Ok(status), Ok((out, err))) => {
                let code = status.code().unwrap_or(-1);
                let body = if output == OutputMode::Stdout && status.success() {
                    out.noted()
                } else {
                    crate::bash::body(&out, &err)
                };
                if body.is_empty() {
                    format!("exited {code}, no output")
                } else {
                    format!("exited {code}:\n{body}")
                }
            }
            (_, Err(error)) => error.to_string(),
            (Err(error), _) => format!("could not wait for the command: {error}"),
        };
        execution.channel().result(result);
        execution.end();
    });
    Ok(())
}

// A script that never reads fd 3 must not stall command supervision.
async fn tell(mut to: OwnedWriteHalf, mut hear: UnboundedReceiver<Told>) {
    while let Some(told) = hear.recv().await {
        let line = match told {
            Told::Response { request, event } => match event {
                crate::channel::Response::Queued => {
                    serde_json::json!({ "request": request, "queued": true })
                }
                crate::channel::Response::Started => {
                    serde_json::json!({ "request": request, "started": true })
                }
                crate::channel::Response::Progress(text) => {
                    serde_json::json!({ "request": request, "progress": text })
                }
                crate::channel::Response::Finished { text, outcome } => match outcome {
                    crate::channel::Outcome::Done => {
                        serde_json::json!({ "request": request, "reply": text, "status": "done" })
                    }
                    crate::channel::Outcome::Stopped(reason) => {
                        serde_json::json!({
                            "request": request, "reply": text, "status": "stopped", "reason": reason
                        })
                    }
                    crate::channel::Outcome::Failed(error) => {
                        serde_json::json!({
                            "request": request, "reply": text, "status": "failed", "error": error
                        })
                    }
                    crate::channel::Outcome::Exited(code) => {
                        serde_json::json!({
                            "request": request, "reply": text, "exit_code": code,
                            "status": if code == 0 { "done" } else { "failed" }
                        })
                    }
                },
            },
            Told::Started => serde_json::json!({ "started": true }),
            Told::Progress(text) => serde_json::json!({ "progress": text }),
            Told::Reply(text) => serde_json::json!({ "reply": text }),
            Told::Asked => serde_json::json!({ "asked": true }),
            Told::Answer(Ok(picked)) => serde_json::json!({ "answer": picked }),
            Told::Answer(Err(why)) => serde_json::json!({ "answer": null, "error": why }),
            Told::Question { id, asked } => serde_json::json!({ "question": {
                "id": id,
                "text": asked.question,
                "options": asked.options,
                "back": asked.back,
            }}),
            Told::Closed(id) => serde_json::json!({ "closed": id }),
            Told::Refused { question, why } => {
                serde_json::json!({ "refused": question, "why": why })
            }
        };
        if to.write_all(format!("{line}\n").as_bytes()).await.is_err() {
            return;
        }
    }
}

// A closed event channel must not keep winning the supervisor’s select.
struct Fd3 {
    reader: BufReader<OwnedReadHalf>,
    // The line so far, kept here so a read cut short by `select!` loses nothing.
    line: Vec<u8>,
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
    if v.get("follow").and_then(Value::as_bool) == Some(true) {
        return Some(Said::Follow);
    }
    if let Some(question) = text("choose") {
        let options = (v
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten())
        .filter_map(|o| o.as_str().map(str::to_string))
        .collect();
        let back = v.get("answer").and_then(Value::as_str) == Some("back");
        return Some(Said::Choose(Asked {
            question,
            options,
            back,
        }));
    }
    if let Some(question) = v.get("answer").and_then(Value::as_u64) {
        let picks = (v
            .get("picks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten())
        .map(|p| p.as_u64().map(|p| p as usize))
        .collect::<Option<_>>()?;
        return Some(Said::Answer { question, picks });
    }
    text("result")
        .map(Said::Result)
        .or_else(|| text("input").map(Said::Input))
        .or_else(|| text("status").map(Said::Status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash::Bash;
    use crate::channel::{NoOne, Outcome, Response};
    use crate::testing::{Sink, ctx};
    use std::sync::{Arc, Mutex};
    use tool::Tool;
    const SECOND: Duration = Duration::from_secs(1);
    fn script(dir: &std::path::Path, body: &str) -> Command {
        let path = dir.join("script");
        std::fs::write(&path, body).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg(path).current_dir(dir);
        cmd
    }

    #[tokio::test]
    async fn a_busy_event_channel_cannot_hide_the_foreground_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(
            dir.path(),
            "while :; do echo '{\"status\":\"busy\"}' >&3; done",
        );
        let ended = tokio::time::timeout(
            3 * SECOND,
            foreground(
                "busy",
                cmd,
                Vec::new(),
                Duration::from_millis(30),
                &ctx(dir.path()),
                &NoOne,
            ),
        )
        .await
        .expect("the deadline remains observable while events arrive");
        assert!(matches!(ended, Err(ToolError::Timeout { .. })));
    }

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
        foreground("big", cmd, Vec::new(), 5 * SECOND, &ctx, &NoOne)
            .await
            .unwrap();
        assert_eq!(*progress.lock().unwrap(), ["after"]);
    }

    #[tokio::test]
    async fn a_question_asked_in_the_turn_has_its_answer_back() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let body = r#"echo '{"choose":"run it?","options":["allow","deny"],"answer":"back"}' >&3
read -r a <&3
printf '%s\n' "$a" > heard"#;
        let ran = {
            let sink = sink.clone();
            let ctx = ctx.clone();
            let cmd = script(dir.path(), body);
            tokio::spawn(
                async move { foreground("guard", cmd, Vec::new(), SECOND, &ctx, &*sink).await },
            )
        };
        let deadline = std::time::Instant::now() + 5 * SECOND;
        let to = loop {
            if let Some(to) = sink.answer.lock().unwrap().take() {
                break to;
            }
            assert!(std::time::Instant::now() < deadline, "never asked");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(sink.said.lock().unwrap()[0], "ask guard run it?");
        // Past its one-second limit: the clock stood while the person thought.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        to.send(Some("deny".into())).unwrap();
        assert!(ran.await.unwrap().unwrap().status.success());
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        assert_eq!(heard, "{\"answer\":\"deny\"}\n");

        let cmd = script(dir.path(), body);
        foreground("guard", cmd, Vec::new(), 5 * SECOND, &ctx, &NoOne)
            .await
            .unwrap();
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        assert!(heard.starts_with("{\"answer\":null,\"error\":"), "{heard}");
    }

    #[tokio::test]
    async fn bash_background_carries_inputs_questions_and_replies() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let _cmd = script(
            dir.path(),
            r#"echo '{"input":"why did it fail"}' >&3
echo '{"choose":"the task changed","options":["summarize","go on"]}' >&3
echo '{"answer":3,"picks":[1]}' >&3
echo '{"interrupt":true}' >&3
echo '{"follow":true}' >&3
read -r a <&3; read -r b <&3; read -r c <&3; read -r d <&3; read -r e <&3; read -r f <&3
printf '%s\n%s\n%s\n%s\n%s\n%s\n' "$a" "$b" "$c" "$d" "$e" "$f" > heard"#,
        );
        let ended = sink.ended.notified();
        Bash::new(sink.clone())
            .execute(
                serde_json::json!({"command":"sh script", "background":true}),
                &ctx,
            )
            .await
            .unwrap();
        let told = sink.told.lock().unwrap().clone().expect("it listens");
        told.send(Told::Started).unwrap();
        told.send(Told::Progress("checking".into())).unwrap();
        told.send(Told::Reply("parse.rs:40".into())).unwrap();
        let asked = Asked {
            question: "go on?".into(),
            options: vec!["yes".into()],
            back: false,
        };
        told.send(Told::Question { id: 3, asked }).unwrap();
        told.send(Told::Closed(3)).unwrap();
        told.send(Told::Refused {
            question: 3,
            why: "it is no longer up".into(),
        })
        .unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("it ended");
        assert_eq!(
            sink.said.lock().unwrap()[1..6],
            [
                "input why did it fail",
                "choose the task changed: summarize | go on",
                "answer 3: [1]",
                "interrupt",
                "follow"
            ]
        );
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        let heard: Vec<Value> = (heard.lines())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            heard,
            [
                serde_json::json!({ "started": true }),
                serde_json::json!({ "progress": "checking" }),
                serde_json::json!({ "reply": "parse.rs:40" }),
                serde_json::json!({ "question": {
                    "id": 3, "text": "go on?", "options": ["yes"], "back": false,
                }}),
                serde_json::json!({ "closed": 3 }),
                serde_json::json!({ "refused": 3, "why": "it is no longer up" }),
            ]
        );
    }

    #[tokio::test]
    async fn overlapping_operations_keep_their_identity_on_the_wire() {
        let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
        let (_, write) = ours.into_split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let writer = tokio::spawn(tell(write, rx));
        for (request, event) in [
            (7, Response::Queued),
            (7, crate::channel::Response::Started),
            (8, crate::channel::Response::Started),
            (
                8,
                Response::Finished {
                    text: "status".into(),
                    outcome: Outcome::Done,
                },
            ),
            (7, crate::channel::Response::Progress("working".into())),
            (
                7,
                Response::Finished {
                    text: "done".into(),
                    outcome: Outcome::Done,
                },
            ),
            (
                9,
                Response::Finished {
                    text: "stopped".into(),
                    outcome: Outcome::Done,
                },
            ),
            (
                10,
                Response::Finished {
                    text: "partial answer".into(),
                    outcome: Outcome::Stopped(String::new()),
                },
            ),
            (
                11,
                Response::Finished {
                    text: "partial answer".into(),
                    outcome: Outcome::Failed("connection lost".into()),
                },
            ),
            (
                12,
                Response::Finished {
                    text: String::new(),
                    outcome: Outcome::Stopped("run it again".into()),
                },
            ),
            (
                13,
                Response::Finished {
                    text: String::new(),
                    outcome: Outcome::Failed("connection lost".into()),
                },
            ),
            (
                14,
                Response::Finished {
                    text: String::new(),
                    outcome: Outcome::Exited(0),
                },
            ),
            (
                15,
                Response::Finished {
                    text: "exit 3".into(),
                    outcome: Outcome::Exited(3),
                },
            ),
        ] {
            tx.send(Told::Response { request, event }).unwrap();
        }
        drop(tx);
        let mut lines = BufReader::new(theirs).lines();
        for expected in [
            serde_json::json!({"request": 7, "queued": true}),
            serde_json::json!({"request": 7, "started": true}),
            serde_json::json!({"request": 8, "started": true}),
            serde_json::json!({"request": 8, "reply": "status", "status": "done"}),
            serde_json::json!({"request": 7, "progress": "working"}),
            serde_json::json!({"request": 7, "reply": "done", "status": "done"}),
            serde_json::json!({"request": 9, "reply": "stopped", "status": "done"}),
            serde_json::json!({"request": 10, "reply": "partial answer", "status": "stopped", "reason": ""}),
            serde_json::json!({"request": 11, "reply": "partial answer", "status": "failed", "error": "connection lost"}),
            serde_json::json!({"request": 12, "reply": "", "status": "stopped", "reason": "run it again"}),
            serde_json::json!({"request": 13, "reply": "", "status": "failed", "error": "connection lost"}),
            serde_json::json!({"request": 14, "reply": "", "status": "done", "exit_code": 0}),
            serde_json::json!({"request": 15, "reply": "exit 3", "status": "failed", "exit_code": 3}),
        ] {
            let line = lines.next_line().await.unwrap().unwrap();
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
        }
        writer.await.unwrap();
        assert!(lines.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_left_behind_child_holding_stdout_still_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(dir.path(), "sleep 30 &\necho $! > pid");
        let err = foreground("leaky", cmd, Vec::new(), SECOND, &ctx(dir.path()), &NoOne)
            .await
            .err()
            .expect("timed out");
        assert!(matches!(err, ToolError::Timeout { .. }), "{err}");
        assert_gone(&dir.path().join("pid")).await;
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
