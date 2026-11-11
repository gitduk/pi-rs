//! fd 3: one request or notice per line, one reply per request.
//!
//! A line carrying `id` is answered exactly once; one without it never is.
use crate::process::{Exited, Group, MAX_RUN, Streams, past, put_down, reap, spawn};
use crate::runtime::background::{OutputMode, RunHandle, Scope};
use crate::runtime::channel::{Answer, Ask, Asked, Observation, Receiver, Sender, Told};
use crate::scripts::wire::{self, Id, Incoming, Speaker, Stream as Listening};
use ::execution::background::{Outcome, Report};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
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
// A question is answered off the read loop, so the execution's deadline and
// `stop` still land; this only bounds a host that never answers at all.
const ASK_LIMIT: Duration = Duration::from_secs(24 * 3600);

/// What the writer must still answer, shared with the reader.
#[derive(Clone, Default)]
struct Holding {
    // One entry per `say`, in the order they were sent: the id to answer, when
    // that say asked for one.
    says: Arc<Mutex<VecDeque<Option<Id>>>>,
    // The message a turn's remaining words belong to, by its number.
    linked: Arc<Mutex<HashMap<u64, message::MessageId>>>,
    // Questions put to this script, by the id it was asked under.
    asks: Arc<Mutex<HashMap<Id, u64>>>,
}

struct Process {
    child: Child,
    streams: Streams,
    fd3: Fd3,
    told: Sender,
    out: Output,
    holding: Holding,
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
    let holding = Holding::default();
    let (told, hear) = crate::runtime::channel::channel();
    let (out, output) = output();
    tokio::spawn(tell(write, hear, output, holding.clone()));

    Ok(Process {
        child,
        streams,
        fd3,
        told,
        out,
        holding,
        guard,
    })
}

/// Run a foreground capability; its questions go to the person in this turn.
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
        holding: _holding,
        guard: _guard,
    } = start(cmd, input, ctx, None)?;
    let group = child.id();
    let limit = timeout.min(MAX_RUN);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut waiting: Option<(Id, Answer)> = None;
    let mut exit = Box::pin(async { (child.wait().await, streams.checked(group).await) });
    let late = loop {
        tokio::select! {biased;
            ()=ctx.cancel.cancelled()=>break false,
            picked=async {
                let held = waiting.as_mut().expect("guarded");
                (&mut held.1).await
            }, if waiting.is_some()=>{
                let (id, _) = waiting.take().expect("guarded");
                let said = match picked.ok().flatten() {
                    Some(text) => wire::reply(&id, json!({"answer": text})),
                    None => wire::failure(&id, "closed", "the question went away"),
                };
                out.value(&said);
                deadline.as_mut().reset(tokio::time::Instant::now()+limit);
            }
            ()=&mut deadline, if waiting.is_none()=>break true,
            line=fd3.next()=>match line {
                Ok(value)=>waiting=answered_outside(&value,&out,ctx,name,asks,waiting),
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

/// A foreground script may report progress and ask; its answer comes back here.
fn answered_outside(
    value: &Value,
    out: &Output,
    ctx: &Ctx,
    name: &str,
    asks: &dyn Ask,
    waiting: Option<(Id, Answer)>,
) -> Option<(Id, Answer)> {
    let incoming: Incoming = match serde_json::from_value(value.clone()) {
        Ok(incoming) => incoming,
        Err(error) => {
            out.reject(format!("invalid frame: {error}"));
            return waiting;
        }
    };
    match incoming {
        Incoming::Status { text } => {
            ctx.progress(text);
            waiting
        }
        Incoming::Ask {
            id: Some(id),
            text,
            options,
        } => {
            if waiting.is_some() {
                out.value(&wire::failure(
                    &id,
                    "busy",
                    "one question is already waiting",
                ));
                return waiting;
            }
            let asked = Asked {
                question: text,
                options,
                back: true,
            };
            match asks.ask(ctx.into(), name, asked) {
                Ok(Some(answer)) => Some((id, answer)),
                Ok(None) => {
                    out.value(&wire::failure(&id, "closed", "the question went away"));
                    waiting
                }
                Err(why) => {
                    out.value(&wire::failure(&id, "internal", why));
                    waiting
                }
            }
        }
        Incoming::Ask { id: None, .. } => {
            out.value(&wire::unreadable(
                "frame",
                "a foreground question waits for its answer and needs an id",
            ));
            waiting
        }
        _ => {
            out.value(&wire::unreadable(
                "frame",
                "a foreground script may report status and ask",
            ));
            waiting
        }
    }
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
        holding,
        guard,
    } = start(cmd, input, ctx, execution.owner())?;
    execution.watch(streams.peek());
    execution.channel().listen(told);
    tokio::spawn(async move {
        let _guard = guard;
        let group = child.id();
        let mut spoke = false;
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
                line=fd3.next()=>match line {
                    Ok(value)=>{spoke=true; say_out(&value,&out,&holding,&execution).await;}
                    Err(why)=>out.reject(why),
                },
                status=child.wait()=>break status,
            }
        };
        let captures = tokio::select! {biased;
            ()=stop.cancelled()=>{reap(group).await;return;}
            captures=streams.checked(group)=>captures,
        };
        let code = match &status {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };
        let reported = match (status, captures) {
            (Ok(status), Ok((stdout, stderr))) => {
                // A process that never spoke has only its output to speak
                // with; one that spoke owns its words, so only failure reports.
                if !spoke {
                    let body = if output == OutputMode::Stdout && status.success() {
                        stdout.noted()
                    } else {
                        crate::bash::body(&stdout, &stderr)
                    };
                    Some(if body.is_empty() {
                        format!("exited {code}, no output")
                    } else {
                        format!("exited {code}:\n{body}")
                    })
                } else if status.success() {
                    None
                } else {
                    let errors = stderr.noted();
                    Some(if errors.trim().is_empty() {
                        format!("exited {code}, no error output")
                    } else {
                        format!("exited {code}:\n{}", errors.trim_end())
                    })
                }
            }
            (_, Err(error)) => Some(error.to_string()),
            (Err(error), _) => Some(format!("could not wait for the command: {error}")),
        };
        match reported {
            Some(text) if spoke => {
                execution.channel().status(text.clone());
                execution.channel().result(text);
            }
            Some(text) => execution.channel().result(text),
            None => execution.channel().status(format!("exited {code}")),
        }
        execution.end();
    });
    Ok(())
}

/// One line from the script: a request, a notice, or an answer to a question.
async fn say_out(value: &Value, out: &Output, holding: &Holding, execution: &Arc<dyn RunHandle>) {
    if value.get("op").is_none() {
        answered(value, out, execution, holding);
        return;
    }
    let incoming: Incoming = match serde_json::from_value(value.clone()) {
        Ok(incoming) => incoming,
        Err(error) => {
            let id = value
                .get("id")
                .and_then(|id| serde_json::from_value::<Id>(id.clone()).ok());
            let frame = match id {
                Some(id) => wire::failure(&id, "frame", format!("invalid frame: {error}")),
                None => wire::unreadable("frame", format!("invalid frame: {error}")),
            };
            out.value(&frame);
            return;
        }
    };
    if let Err((code, why)) = incoming.validate() {
        match incoming.id() {
            Some(id) => out.value(&wire::failure(id, code, why)),
            None => out.value(&wire::unreadable(code, why)),
        }
        return;
    }
    match incoming {
        Incoming::Initialize { id, protocol } => {
            if protocol == wire::PROTOCOL {
                out.value(&wire::reply(&id, json!({"protocol": wire::PROTOCOL})));
            } else {
                out.value(&wire::failure(
                    &id,
                    "protocol",
                    format!(
                        "this pi speaks {}; the script asked for {protocol}",
                        wire::PROTOCOL
                    ),
                ));
            }
        }
        Incoming::Say {
            id,
            text,
            speaker,
            note,
            more,
            re,
            stop,
            source,
            reason,
        } => {
            if let Some(re) = re {
                match execution.channel().respond(re, text, stop) {
                    Ok(_) => {
                        if let Some(id) = id {
                            out.value(&wire::reply(&id, json!({"accepted": true})));
                        }
                    }
                    Err(why) => {
                        if let Some(id) = id {
                            out.value(&wire::failure(&id, "internal", why));
                        } else {
                            out.reject(why);
                        }
                    }
                }
                return;
            }
            execution.channel().drive(more);
            match speaker {
                Speaker::Person => {
                    // The person's words, not the person's authority: a script
                    // relays what was said, never a command for this machine.
                    if text.starts_with('!') {
                        if let Some(id) = id {
                            out.value(&wire::failure(
                                &id,
                                "denied",
                                "a line from a script may not run shell commands",
                            ));
                        } else {
                            out.reject("a line from a script may not run shell commands");
                        }
                        return;
                    }
                    holding.says.lock().unwrap().push_back(id.clone());
                    execution.channel().input(text, note);
                }
                Speaker::Task => match execution.channel().task(text, source, reason) {
                    Ok(_) => {
                        if let Some(id) = id {
                            out.value(&wire::reply(&id, json!({"accepted": true})));
                        }
                    }
                    Err(why) => match id {
                        Some(id) => out.value(&wire::failure(&id, "internal", why)),
                        None => out.reject(why),
                    },
                },
            }
        }
        Incoming::Ask { id, text, options } => match id {
            Some(id) => {
                let asked = Asked {
                    question: text,
                    options,
                    back: true,
                };
                match execution.channel().ask(asked) {
                    // Answered off this loop: a person may think for as long as
                    // the execution's own deadline allows, and `stop` still lands.
                    Ok(answer) => {
                        let out = out.clone();
                        drop(tokio::spawn(async move {
                            let said = match tokio::time::timeout(ASK_LIMIT, answer).await {
                                Ok(Ok(Some(text))) => wire::reply(&id, json!({"answer": text})),
                                Ok(Ok(None)) => {
                                    wire::failure(&id, "closed", "the question went away")
                                }
                                Ok(Err(_)) => wire::failure(&id, "closed", "the asker is gone"),
                                Err(_) => wire::failure(&id, "closed", "the question timed out"),
                            };
                            out.value(&said);
                        }));
                    }
                    Err(why) => out.value(&wire::failure(&id, "internal", why)),
                }
            }
            None => execution.channel().choose(Asked {
                question: text,
                options,
                back: false,
            }),
        },
        Incoming::Listen { id, to, cursor } => match to {
            Listening::Session => match execution.owner().map(|owner| owner.session) {
                Some(session) => match message::SessionId::new(session)
                    .map_err(|e| e.to_string())
                    .and_then(|session| execution.channel().observe(session, cursor))
                {
                    Ok(observation) => {
                        out.value(&wire::reply(&id, json!({"cursor": observation.cursor()})));
                        out.observe(observation);
                    }
                    Err(why) => out.value(&wire::failure(&id, "internal", why)),
                },
                None => out.value(&wire::failure(
                    &id,
                    "internal",
                    "this execution captured no session to listen to",
                )),
            },
            Listening::Questions => {
                execution.channel().watch_prompts();
                out.value(&wire::reply(&id, json!({})));
            }
        },
        Incoming::Status { text } => execution.channel().status(text),
        Incoming::Stop {} => execution.channel().interrupt(),
    }
}

/// A reply to a question this script was asked.
fn answered(value: &Value, out: &Output, execution: &Arc<dyn RunHandle>, holding: &Holding) {
    let id = match value.get("id") {
        Some(id) => match serde_json::from_value::<Id>(id.clone()) {
            Ok(id) => id,
            Err(_) => {
                out.reject("a reply needs a usable id");
                return;
            }
        },
        None => {
            out.reject("a reply needs an id");
            return;
        }
    };
    let Some(question) = holding.asks.lock().unwrap().remove(&id) else {
        out.value(&wire::failure(
            &id,
            "closed",
            "no question is waiting under that id",
        ));
        return;
    };
    if let Some(why) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        out.value(&wire::refused(question, why));
        return;
    }
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    if let Some(picks) = result.get("picks").and_then(Value::as_array) {
        let picks: Vec<usize> = (picks.iter())
            .filter_map(Value::as_u64)
            .map(|n| n as usize)
            .collect();
        execution.channel().answer(question, picks);
    } else if let Some(text) = result.get("text").and_then(Value::as_str) {
        execution.channel().answer_text(question, text.to_string());
    } else {
        out.value(&wire::failure(
            &id,
            "frame",
            "an answer carries picks or text",
        ));
    }
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
    fn reject(&self, why: impl Into<String>) {
        let why = why.into();
        tracing::warn!(target: "pi::scripts", %why, "frame rejected");
        self.value(&wire::unreadable("frame", why));
    }
    fn observe(&self, observation: Box<dyn Observation>) {
        self.put(Outgoing::Observe(observation), 1);
    }
}

async fn write(to: &mut OwnedWriteHalf, value: &impl Serialize) -> Result<(), String> {
    let bytes = wire::encode(value)?;
    tokio::time::timeout(WRITE_TIMEOUT, to.write_all(&bytes))
        .await
        .map_err(|_| "frame receiver stopped reading".to_string())?
        .map_err(|e| e.to_string())
}
async fn tell(mut to: OwnedWriteHalf, mut hear: Receiver, mut output: OutputIn, holding: Holding) {
    let mut observation: Option<Box<dyn Observation>> = None;
    let disconnected = hear.disconnection();
    loop {
        tokio::select! {biased;
            ()=output.closed.cancelled()=>return,
            ()=disconnected.cancelled()=>return,
            frame=output.from.recv()=>match frame {
                None=>return,
                Some(queued)=>match queued.frame {
                    Outgoing::Bytes(bytes)=>if !matches!(tokio::time::timeout(WRITE_TIMEOUT,to.write_all(&bytes)).await,Ok(Ok(()))){return},
                    Outgoing::Observe(next)=>{observation=Some(next);}
                },
            },
            event=hear.recv()=>{
                let Some(event)=event else{return};
                let frames: Vec<Value> = match event {
                    Told::Linked { operation, request }=>{
                        holding.linked.lock().unwrap().insert(operation, request);
                        Vec::new()
                    }
                    Told::Response { request, event }=>match event {
                        ::execution::channel::Response::Finished { text, outcome }=>{
                            let linked=holding.linked.lock().unwrap().remove(&request);
                            let id=holding.says.lock().unwrap().pop_front().flatten();
                            match id {
                                Some(id)=>vec![wire::reply(&id, json!({
                                    "turn": request,
                                    "state": state(&outcome),
                                    "text": text,
                                    "message": linked,
                                }))],
                                None=>Vec::new(),
                            }
                        }
                        _=>Vec::new(),
                    },
                    Told::Question { id, asked, request }=>match request {
                        Some(_)=>Vec::new(),
                        None=>{
                            let ask_id=Id::Number(id);
                            holding.asks.lock().unwrap().insert(ask_id.clone(), id);
                            vec![wire::ask(&ask_id, &asked.question, &asked.options)]
                        }
                    },
                    Told::Closed(question)=>{
                        let mut asks=holding.asks.lock().unwrap();
                        let id=(asks.iter()).find(|(_, q)| **q == question).map(|(id, _)| id.clone());
                        match id {
                            Some(id)=>{asks.remove(&id); vec![wire::failure(&id, "closed", "the question went away")]}
                            None=>Vec::new(),
                        }
                    }
                    Told::Refused { question, why }=>vec![wire::refused(question, &why)],
                    Told::Started | Told::Progress(_) | Told::Reply(_) | Told::Asked | Told::Answer(_)=>Vec::new(),
                };
                for frame in frames {
                    if write(&mut to,&frame).await.is_err(){return}
                }
            },
            event=async {observation.as_mut().expect("guarded").next().await},if observation.is_some()=>match event {
                Ok(event)=>{
                    let frame=wire::message(&event.cursor,event.message.as_ref());
                    if write(&mut to,&frame).await.is_err(){return}
                }
                Err(why)=>{observation=None;if write(&mut to,&wire::unreadable("internal",why)).await.is_err(){return}}
            },
        }
    }
}

fn state(outcome: &crate::runtime::channel::Outcome) -> &'static str {
    use crate::runtime::channel::Outcome;
    match outcome {
        Outcome::Done | Outcome::Exited(0) => "done",
        Outcome::Stopped(_) => "stopped",
        Outcome::Failed(_) | Outcome::Exited(_) => "failed",
    }
}

struct Fd3 {
    reader: BufReader<OwnedReadHalf>,
    line: Vec<u8>,
    skipping: bool,
    open: bool,
}
impl Fd3 {
    async fn next(&mut self) -> Result<Value, String> {
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
    use crate::runtime::channel::{NoOne, Outcome, Response};
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
            "while :; do echo '{\"op\":\"status\",\"text\":\"busy\"}' >&3; done",
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

    #[tokio::test]
    async fn an_overlong_line_on_fd_3_is_dropped_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let seen = progress.clone();
        let ctx = ctx(dir.path()).with_progress(Arc::new(move |t| seen.lock().unwrap().push(t)));
        let cmd = script(
            dir.path(),
            &format!(
                "head -c {} /dev/zero | tr '\\0' x >&3; echo >&3; echo '{{\"op\":\"status\",\"text\":\"after\"}}' >&3",
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
        let body = r#"echo '{"id":"a1","op":"ask","text":"run it?","options":["allow","deny"]}' >&3
read -r a <&3
printf '%s\n' "$a" > heard"#;
        let ran = {
            let sink = sink.clone();
            let ctx = ctx.clone();
            let cmd = script(dir.path(), body);
            tokio::spawn(async move {
                foreground("guard", cmd, Vec::new(), 5 * SECOND, &ctx, &*sink).await
            })
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
        to.send(Some("deny".into())).unwrap();
        assert!(ran.await.unwrap().unwrap().status.success());
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        let frame: Value = serde_json::from_str(&heard).unwrap();
        assert_eq!(frame, json!({"id":"a1","result":{"answer":"deny"}}));
    }

    #[tokio::test]
    async fn bash_background_carries_says_asks_and_notices() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let _cmd = script(
            dir.path(),
            r#"echo '{"id":1,"op":"initialize","protocol":4}' >&3
echo '{"id":2,"op":"say","text":"host operation","note":"round 2","more":true}' >&3
echo '{"op":"ask","text":"the task changed","options":["summarize","go on"]}' >&3
echo '{"id":3,"op":"listen","to":"questions"}' >&3
echo '{"op":"status","text":"working"}' >&3
echo '{"op":"stop"}' >&3
read -r a <&3; read -r b <&3; read -r c <&3
printf '%s\n%s\n%s\n' "$a" "$b" "$c" > heard"#,
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
        let deadline = std::time::Instant::now() + 5 * SECOND;
        while !sink
            .said
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("input host operation"))
        {
            assert!(std::time::Instant::now() < deadline, "never said");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        told.send(Told::Response {
            request: 7,
            event: Response::Finished {
                text: "parse.rs:40".into(),
                outcome: Outcome::Done,
            },
        })
        .unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("it ended");
        let said = sink.said.lock().unwrap().clone();
        assert!(said.contains(&"drive true".to_string()), "{said:?}");
        assert!(
            said.contains(&"input host operation (round 2)".to_string()),
            "{said:?}"
        );
        assert!(said.contains(&"watch".to_string()), "{said:?}");
        assert!(said.contains(&"status working".to_string()), "{said:?}");
        assert!(said.contains(&"interrupt".to_string()), "{said:?}");
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        let heard: Vec<Value> = (heard.lines())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(
            heard.contains(&json!({"id":1,"result":{"protocol":4}})),
            "{heard:?}"
        );
        assert!(heard.contains(&json!({"id":3,"result":{}})), "{heard:?}");
        assert!(
            heard.contains(&json!({"id":2,"result":{"turn":7,"state":"done","text":"parse.rs:40","message":null}})),
            "{heard:?}"
        );
    }

    #[tokio::test]
    async fn a_scripts_line_cannot_run_a_shell_command() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let _cmd = script(
            dir.path(),
            r#"echo '{"id":1,"op":"say","text":"!rm -rf /tmp/x"}' >&3
read -r a <&3
printf '%s\n' "$a" > heard"#,
        );
        let ended = sink.ended.notified();
        Bash::new(sink.clone())
            .execute(
                serde_json::json!({"command":"sh script", "background":true}),
                &ctx,
            )
            .await
            .unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("it ended");
        let said = sink.said.lock().unwrap().clone();
        assert!(
            !said.iter().any(|s| s.starts_with("input ")),
            "the line never reached the session: {said:?}"
        );
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        let frame: Value = serde_json::from_str(&heard).unwrap();
        assert_eq!(frame["id"], 1);
        assert_eq!(frame["error"]["code"], "denied", "{frame}");
    }

    #[tokio::test]
    async fn a_script_that_never_spoke_reports_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let _cmd = script(dir.path(), "echo built; exit 0");
        let ended = sink.ended.notified();
        Bash::new(sink.clone())
            .execute(
                serde_json::json!({"command":"sh script", "background":true}),
                &ctx,
            )
            .await
            .unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("it ended");
        let said = sink.said.lock().unwrap().clone();
        assert!(
            said.iter().any(|s| s.starts_with("result exited 0")),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_script_reports_its_error_but_a_clean_one_only_its_status() {
        for (body, reported) in [("echo boom >&2; exit 3", true), ("exit 0", false)] {
            let dir = tempfile::tempdir().unwrap();
            let sink = Arc::new(Sink::default());
            let ctx = ctx(dir.path());
            let _cmd = script(
                dir.path(),
                &format!("echo '{{\"op\":\"status\",\"text\":\"working\"}}' >&3\n{body}"),
            );
            let ended = sink.ended.notified();
            Bash::new(sink.clone())
                .execute(
                    serde_json::json!({"command":"sh script", "background":true}),
                    &ctx,
                )
                .await
                .unwrap();
            tokio::time::timeout(5 * SECOND, ended)
                .await
                .expect("it ended");
            let said = sink.said.lock().unwrap().clone();
            let got = said.iter().any(|s| s.starts_with("result exited"));
            assert_eq!(got, reported, "{said:?}");
            if reported {
                assert!(
                    said.iter().any(|s| s.contains("boom")),
                    "the failure says why: {said:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn answering_a_question_put_to_this_script_reaches_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let _cmd = script(
            dir.path(),
            r#"echo '{"id":1,"op":"listen","to":"questions"}' >&3
read -r a <&3
printf '%s\n' "$a" > /dev/null
echo '{"id":3,"result":{"picks":[1]}}' >&3
sleep 1"#,
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
        let asked = Asked {
            question: "the task changed".into(),
            options: vec!["summarize".into(), "go on".into()],
            back: false,
        };
        told.send(Told::Question {
            id: 3,
            asked,
            request: None,
        })
        .unwrap();
        tokio::time::timeout(5 * SECOND, ended)
            .await
            .expect("it ended");
        let said = sink.said.lock().unwrap().clone();
        assert!(said.iter().any(|s| s == "answer 3: [1]"), "{said:?}");
    }
}
