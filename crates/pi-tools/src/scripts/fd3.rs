//! Duplex fd 3 frames, with semantic observation pulled directly by the writer.
use crate::process::{Exited, Group, MAX_RUN, Streams, past, put_down, reap, spawn};
use crate::runtime::background::{OutputMode, RunHandle, Scope};
use crate::runtime::channel::{Answer, Ask, Observation, Receiver, Sender, Told};
use crate::scripts::wire::{self, Command as Control, Incoming};
use ::execution::background::{Outcome, Report};
use serde::Serialize;
#[cfg(test)]
use serde_json::Value;
use serde_json::json;
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tool::{Ctx, ToolError, ToolOutput};

pub const FD_VAR: &str = "PI_EVENTS_FD";
const FD: i32 = 3;
// Detach a peer that stops draining its frames.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LINE: usize = wire::MAX_FRAME_BYTES + 1;

struct Process {
    child: Child,
    streams: Streams,
    fd3: Fd3,
    told: Sender,
    out: Output,
    guard: Option<Group>,
}

fn start(
    mut cmd: Command,
    input: Vec<u8>,
    ctx: &Ctx,
    owner: Option<crate::runtime::background::Owner>,
) -> Result<Process, ToolError> {
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
    if let Some(owner) = owner.or_else(|| Scope::from(ctx).owner) {
        cmd.env(wire::SESSION_VAR, owner.session);
        match owner.request {
            Some(id) => {
                cmd.env(wire::REQUEST_VAR, id.as_str());
            }
            None => {
                cmd.env_remove(wire::REQUEST_VAR);
            }
        }
    } else {
        cmd.env_remove(wire::SESSION_VAR);
        cmd.env_remove(wire::REQUEST_VAR);
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
    let (told, hear) = crate::runtime::channel::channel();
    let (out, output) = output();
    tokio::spawn(tell(write, hear, output));

    Ok(Process {
        child,
        streams,
        fd3,
        told,
        out,
        guard,
    })
}

/// Run a foreground capability; its prompts remain internal runtime interactions.
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
        told: _told,
        out,
        guard: _guard,
    } = start(cmd, input, ctx, None)?;
    let group = child.id();
    let limit = timeout.min(MAX_RUN);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut waiting: Option<Answer> = None;
    let mut exit = Box::pin(async { (child.wait().await, streams.checked(group).await) });
    let late = loop {
        tokio::select! {biased;
            ()=ctx.cancel.cancelled()=>break false,
            picked=async {waiting.as_mut().expect("guarded").await}, if waiting.is_some()=>{
                waiting=None;
                out.runtime(Told::Answer(Ok(picked.ok().flatten())));
                deadline.as_mut().reset(tokio::time::Instant::now()+limit);
            }
            ()=&mut deadline, if waiting.is_none()=>break true,
            frame=fd3.next()=> match frame {
                Ok(Incoming::Runtime {command:Control::Status {text}})=>ctx.progress(text),
                Ok(Incoming::Runtime {command:Control::Prompt {text,options,back}})=>{
                    let put=if waiting.is_some() && back {Err("one prompt is already waiting".into())} else {asks.ask(ctx.into(),name,Control::asked(text,options,back))};
                    match put {Ok(Some(answer))=>waiting=Some(answer), Ok(None)=>out.runtime(Told::Asked), Err(why)=>out.runtime(Told::Answer(Err(why)))}
                }
                Ok(Incoming::Message {..}) | Ok(Incoming::Observe {..}) | Ok(Incoming::Runtime {command:Control::Command {..} | Control::Answer {..} | Control::AnswerText {..} | Control::Serve {} | Control::Interrupt {} | Control::Watch {} | Control::Drive {..}})=>out.reject("this frame requires a background execution"),
                Err(why)=>out.reject(why),
            },
            (status,captures)=&mut exit=>{let (stdout,stderr)=captures?;return Ok(Exited {status:status?,stdout,stderr});}
        }
    };
    drop(exit);
    put_down(&mut child, group).await;
    if !late {
        return Err(ToolError::Cancelled);
    }
    Err(ToolError::Timeout {
        ms: limit.as_millis() as u64,
        printed: streams.printed().await,
    })
}

pub(crate) fn background(
    cmd: Command,
    input: Vec<u8>,
    ctx: &Ctx,
    execution: Arc<dyn RunHandle>,
    stop: CancellationToken,
    output: OutputMode,
    timeout: Option<Duration>,
) -> Result<(), ToolError> {
    let Process {
        mut child,
        mut streams,
        mut fd3,
        told,
        out,
        guard,
    } = start(cmd, input, ctx, execution.owner())?;
    execution.watch(streams.peek());
    execution.channel().listen(told);
    tokio::spawn(async move {
        let _guard = guard;
        let group = child.id();
        let mut service = false;
        let limit = timeout.map(|d| d.min(MAX_RUN));
        let expires = limit.map(|d| tokio::time::Instant::now() + d);
        let status = loop {
            tokio::select! {biased;
                ()=stop.cancelled()=>{put_down(&mut child,group).await;return;}
                _=past(expires)=>{
                    // A command that exited at the deadline keeps its own verdict.
                    if let Ok(Some(status))=child.try_wait(){break Ok(status);}
                    put_down(&mut child,group).await;
                    let ms=limit.expect("a deadline arm has one").as_millis() as u64;
                    let why=format!("{} (the command did not exit on its own)",
                        ToolError::Timeout{ms,printed:streams.printed().await});
                    let report=Report{outcome:Outcome::Failed,output:ToolOutput::text(why).warned()};
                    execution.complete(report);
                    return;
                }
                frame=fd3.next()=>match frame {
                    Ok(Incoming::Message {message})=>{
                        let id=message.id.clone();
                        let request=match &message.kind {message::WireKind::Request(_)=>id.clone(),message::WireKind::Response {request,..}=>request.clone()};
                        match execution.channel().message(message) {
                            Ok(receipt)=>out.accepted(&id,&receipt), Err(why)=>out.value(&json!({"type":"error","id":id,"request":request,"message":why})),
                        }
                    },
                    Ok(Incoming::Observe {session,cursor})=>match execution.channel().observe(session,cursor) {
                        Ok(observation)=>out.observe(observation), Err(why)=>out.error(why),
                    },
                    Ok(Incoming::Runtime {command})=>match command {
                        Control::Serve {}=>if !service {service=true;tracing::info!(target:"pi::scripts",execution=execution.id(),"channel service declared");},
                        Control::Status {text}=>execution.channel().status(text),
                        Control::Command {line}=>execution.channel().input(line,String::new()),
                        Control::Prompt {back:true,..}=>out.reject("a background execution cannot wait for an internal foreground prompt"),
                        Control::Prompt {text,options,back:false}=>execution.channel().choose(Control::asked(text,options,false)),
                        Control::Answer {question,picks}=>execution.channel().answer(question,picks),
                        Control::AnswerText {question,text}=>execution.channel().answer_text(question,text),
                        Control::Interrupt {}=>execution.channel().interrupt(),
                        Control::Watch {}=>execution.channel().watch_prompts(),
                        Control::Drive {enabled}=>execution.channel().drive(enabled),
                    },
                    Err(why)=>out.reject(why),
                },
                status=child.wait()=>break status,
            }
        };
        let captures = tokio::select! {biased;
            ()=stop.cancelled()=>{reap(group).await;return;}
            captures=streams.checked(group)=>captures,
        };
        let result = match (status, captures) {
            (Ok(status), Ok((stdout, stderr))) => {
                let code = status.code().unwrap_or(-1);
                let body = if output == OutputMode::Stdout && status.success() {
                    stdout.noted()
                } else {
                    crate::bash::body(&stdout, &stderr)
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
        if !service {
            execution.channel().result(result);
        } else {
            execution.channel().status(result);
        }
        execution.end();
    });
    Ok(())
}

#[derive(Clone)]
struct Output {
    to: tokio::sync::mpsc::Sender<Queued>,
    budget: Arc<tokio::sync::Semaphore>,
    closed: CancellationToken,
}
struct Queued {
    frame: Outgoing,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
enum Outgoing {
    Bytes(Vec<u8>),
    Observe(Box<dyn Observation>),
}
struct OutputIn {
    from: tokio::sync::mpsc::Receiver<Queued>,
    closed: CancellationToken,
}
fn output() -> (Output, OutputIn) {
    let (to, from) = tokio::sync::mpsc::channel(crate::runtime::channel::EVENT_COUNT);
    let closed = CancellationToken::new();
    (
        Output {
            to,
            budget: Arc::new(tokio::sync::Semaphore::new(
                crate::runtime::channel::EVENT_BYTES,
            )),
            closed: closed.clone(),
        },
        OutputIn { from, closed },
    )
}
impl Output {
    fn put(&self, frame: Outgoing, bytes: usize) {
        let permit = if bytes <= crate::runtime::channel::EVENT_BYTES {
            self.budget
                .clone()
                .try_acquire_many_owned(bytes as u32)
                .ok()
        } else {
            None
        };
        if let Some(permit) = permit
            && self
                .to
                .try_send(Queued {
                    frame,
                    _permit: permit,
                })
                .is_ok()
        {
            return;
        }
        self.closed.cancel();
    }
    fn value(&self, value: &impl Serialize) {
        match wire::encode(value) {
            Ok(bytes) => {
                let size = bytes.len();
                self.put(Outgoing::Bytes(bytes), size)
            }
            Err(why) => {
                tracing::warn!(target:"pi::scripts",%why,"cannot encode frame");
                self.closed.cancel();
            }
        }
    }
    fn error(&self, why: impl Into<String>) {
        self.value(&wire::error(why));
    }
    fn reject(&self, why: impl Into<String>) {
        let why = why.into();
        tracing::warn!(target: "pi::scripts", %why, "frame rejected");
        self.error(why);
    }
    fn runtime(&self, told: Told) {
        if let Some(value) = wire::runtime(told) {
            self.value(&value)
        }
    }
    fn observe(&self, observation: Box<dyn Observation>) {
        self.put(Outgoing::Observe(observation), 1);
    }
    fn accepted(&self, id: &message::MessageId, receipt: &crate::runtime::channel::Receipt) {
        #[derive(Serialize)]
        struct Accepted<'a> {
            r#type: &'static str,
            id: &'a message::MessageId,
            request: &'a message::RequestId,
            cursor: &'a crate::runtime::channel::Cursor,
            response: Option<&'a message::Message>,
        }
        self.value(&Accepted {
            r#type: "accepted",
            id,
            request: &receipt.request,
            cursor: &receipt.cursor,
            response: receipt.response.as_deref(),
        });
    }
}

async fn write(to: &mut OwnedWriteHalf, value: &impl Serialize) -> Result<(), String> {
    let bytes = wire::encode(value)?;
    tokio::time::timeout(WRITE_TIMEOUT, to.write_all(&bytes))
        .await
        .map_err(|_| "frame receiver stopped reading".to_string())?
        .map_err(|e| e.to_string())
}
async fn tell(mut to: OwnedWriteHalf, mut hear: Receiver, mut output: OutputIn) {
    let mut observation: Option<Box<dyn Observation>> = None;
    let mut model_operations = std::collections::HashSet::new();
    let disconnected = hear.disconnection();
    loop {
        tokio::select! {biased;
            ()=output.closed.cancelled()=>return,
            ()=disconnected.cancelled()=>return,
            frame=output.from.recv()=>match frame {
                None=>return,
                Some(queued)=>match queued.frame {
                    Outgoing::Bytes(bytes)=>if !matches!(tokio::time::timeout(WRITE_TIMEOUT,to.write_all(&bytes)).await,Ok(Ok(()))){return},
                    Outgoing::Observe(next)=>{if write(&mut to,&json!({"type":"observing","cursor":next.cursor()})).await.is_err(){return} observation=Some(next);}
                },
            },
            event=hear.recv()=>{
                let Some(mut event)=event else{return};
                let mut model_final=false;
                match &mut event {
                    Told::Linked {operation,..}=>{model_operations.insert(*operation);},
                    Told::Response {request,event} if model_operations.contains(request)=>match event {
                        crate::runtime::channel::Response::Progress(_)=>continue,
                        crate::runtime::channel::Response::Finished {text,..}=>{text.clear();model_operations.remove(request);model_final=true;},
                        crate::runtime::channel::Response::Queued | crate::runtime::channel::Response::Started=>{},
                    },
                    Told::Response {..} | Told::Started | Told::Progress(_) | Told::Reply(_) | Told::Asked | Told::Answer(_) | Told::Question {..} | Told::Closed(_) | Told::Refused {..}=>{},
                }
                if let Some(mut value)=wire::runtime(event) {
                    if model_final {value["event"]["model"]=json!(true);}
                    if write(&mut to,&value).await.is_err(){return}
                }
            },
            event=async {observation.as_mut().expect("guarded").next().await},if observation.is_some()=>match event {
                Ok(event)=>{
                    #[derive(Serialize)] struct MessageFrame<'a>{r#type:&'static str,cursor:&'a crate::runtime::channel::Cursor,message:&'a message::Message}
                    if write(&mut to,&MessageFrame {r#type:"message",cursor:&event.cursor,message:&event.message}).await.is_err(){return}
                }
                Err(why)=>{observation=None;if write(&mut to,&wire::error(why)).await.is_err(){return}}
            },
        }
    }
}

struct Fd3 {
    reader: BufReader<OwnedReadHalf>,
    line: Vec<u8>,
    skipping: bool,
    open: bool,
}
impl Fd3 {
    async fn next(&mut self) -> Result<Incoming, String> {
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
                continue;
            }
            let line = std::mem::take(&mut self.line);
            if std::mem::take(&mut self.skipping) || line.len() > MAX_LINE {
                return Err("frame exceeds the byte limit".into());
            }
            if line.is_empty() && ended {
                return Err("frame connection closed".into());
            }
            return serde_json::from_slice(&line).map_err(|e| format!("invalid frame: {e}"));
        }
        std::future::pending().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash::Bash;
    use crate::runtime::channel::{Asked, NoOne, Outcome, Response};
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
            "while :; do echo '{\"type\":\"runtime\",\"command\":{\"type\":\"status\",\"text\":\"busy\"}}' >&3; done",
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

    // A timed-out script is waited for before its group is swept, so the
    // timeout neither sits out the sweep nor loses what was printed.
    #[tokio::test]
    async fn a_foreground_timeout_keeps_output_and_returns_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(dir.path(), "echo early; sleep 30");
        let started = std::time::Instant::now();
        let ended = foreground(
            "slow",
            cmd,
            Vec::new(),
            Duration::from_millis(300),
            &ctx(dir.path()),
            &NoOne,
        )
        .await;
        assert!(started.elapsed() < 2 * SECOND, "{:?}", started.elapsed());
        match ended {
            Err(ToolError::Timeout { printed, .. }) => {
                assert!(printed.contains("early"), "{printed}")
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a 30s script finished within its 300ms"),
        }
    }

    // An event arriving after one pipe has closed must not re-poll that
    // pipe's finished drain.
    #[tokio::test]
    async fn an_event_after_one_pipe_closes_does_not_break_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(
            dir.path(),
            "exec 1>&-; (sleep 0.2; echo '{\"type\":\"runtime\",\"command\":{\"type\":\"status\",\"text\":\"late\"}}' >&3; sleep 0.2) & exit 0",
        );
        let ended = foreground(
            "late",
            cmd,
            Vec::new(),
            10 * SECOND,
            &ctx(dir.path()),
            &NoOne,
        )
        .await
        .unwrap();
        assert!(ended.status.success());
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
                "head -c {} /dev/zero | tr '\\0' x >&3; echo >&3; echo '{{\"type\":\"runtime\",\"command\":{{\"type\":\"status\",\"text\":\"after\"}}}}' >&3",
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
        let body = r#"echo '{"type":"runtime","command":{"type":"prompt","text":"run it?","options":["allow","deny"],"back":true}}' >&3
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
        let frame: Value = serde_json::from_str(&heard).unwrap();
        assert_eq!(
            frame,
            json!({"type":"runtime","event":{"type":"answer","text":"deny"}})
        );

        let cmd = script(dir.path(), body);
        foreground("guard", cmd, Vec::new(), 5 * SECOND, &ctx, &NoOne)
            .await
            .unwrap();
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        let frame: Value = serde_json::from_str(&heard).unwrap();
        assert_eq!(frame["event"]["type"], "error");
        assert!(frame["event"]["message"].is_string());
    }

    #[tokio::test]
    async fn bash_background_carries_inputs_questions_and_replies() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let _cmd = script(
            dir.path(),
            r#"echo '{"type":"runtime","command":{"type":"drive","enabled":true}}' >&3
echo '{"type":"runtime","command":{"type":"command","line":"host operation"}}' >&3
echo '{"type":"runtime","command":{"type":"prompt","text":"the task changed","options":["summarize","go on"],"back":false}}' >&3
echo '{"type":"runtime","command":{"type":"answer","question":3,"picks":[1]}}' >&3
echo '{"type":"runtime","command":{"type":"interrupt"}}' >&3
echo '{"type":"runtime","command":{"type":"watch"}}' >&3
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
        told.send(Told::Question {
            id: 3,
            asked,
            request: None,
        })
        .unwrap();
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
            sink.said.lock().unwrap()[1..7],
            [
                "drive true",
                "input host operation",
                "choose the task changed: summarize | go on",
                "answer 3: [1]",
                "interrupt",
                "watch"
            ]
        );
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        let heard: Vec<Value> = (heard.lines())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(heard[0]["type"], "runtime");
        assert_eq!(
            heard
                .iter()
                .map(|v| v["event"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "started", "progress", "finished", "prompt", "closed", "refused"
            ]
        );
        assert_eq!(heard[2]["event"]["text"], "parse.rs:40");
        assert_eq!(heard[3]["event"]["question"], 3);
    }

    #[tokio::test]
    async fn overlapping_operations_keep_their_identity_on_the_wire() {
        let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
        let (_, write) = ours.into_split();
        let (tx, rx) = crate::runtime::channel::channel();
        let (_out, output) = output();
        let writer = tokio::spawn(tell(write, rx, output));
        for (request, event) in [
            (7, Response::Queued),
            (7, crate::runtime::channel::Response::Started),
            (8, crate::runtime::channel::Response::Started),
            (
                8,
                Response::Finished {
                    text: "status".into(),
                    outcome: Outcome::Done,
                },
            ),
            (
                7,
                crate::runtime::channel::Response::Progress("working".into()),
            ),
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
        for (id, phase, text, status) in [
            (7, "queued", None, None),
            (7, "started", None, None),
            (8, "started", None, None),
            (8, "finished", Some("status"), Some("done")),
            (7, "progress", Some("working"), None),
            (7, "finished", Some("done"), Some("done")),
            (9, "finished", Some("stopped"), Some("done")),
            (10, "finished", Some("partial answer"), Some("stopped")),
            (11, "finished", Some("partial answer"), Some("failed")),
            (12, "finished", Some(""), Some("stopped")),
            (13, "finished", Some(""), Some("failed")),
            (14, "finished", Some(""), Some("done")),
            (15, "finished", Some("exit 3"), Some("failed")),
        ] {
            let line = lines.next_line().await.unwrap().unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(frame["type"], "runtime");
            assert_eq!(frame["event"]["type"], "operation");
            assert_eq!(frame["event"]["operation"], id);
            let value = &frame["event"]["phase"];
            assert_eq!(value["type"], phase);
            if let Some(text) = text {
                assert_eq!(value["text"], text)
            }
            if let Some(status) = status {
                assert_eq!(value["status"]["type"], status)
            }
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
