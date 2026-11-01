use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent::session::{Entry, Prompt, Session};
use agent::{Agent, AgentError, Event, Retry};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use llm::message::{AssistantContent, Message};
use llm::request::Request;
use llm::stream::{BlockKind, StopReason, StreamEvent, Usage};
use tokio::sync::mpsc;
use tool::{Ctx, Workspace};

mod common;

struct Controlled {
    replies: Mutex<VecDeque<BoxStream<'static, llm::Result<StreamEvent>>>>,
    requests: mpsc::UnboundedSender<Request>,
}

#[async_trait]
impl llm::Transport for Controlled {
    async fn stream(
        &self,
        _: &llm::model::ModelSpec,
        req: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        self.requests.send(req.clone()).unwrap();
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted reply"))
    }
}

fn response(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            delta: text.into(),
        },
        StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]
}

fn prompt(text: &str) -> Prompt {
    Prompt {
        text: text.into(),
        images: Vec::new(),
        shown: None,
        relayed: None,
    }
}

struct Pausing {
    release: tokio::sync::Notify,
    passes: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl agent::Compactor for Pausing {
    async fn compact(
        &self,
        session: &mut Session,
        _: agent::Working<'_>,
        _: usize,
        _: bool,
        _: &agent::event::Voice,
    ) -> agent::Fitted {
        if self
            .passes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            self.release.notified().await;
        }
        agent::Fitted {
            context: session.context(),
            changed: false,
            spent: Usage {
                input: 7,
                ..Default::default()
            },
        }
    }
}

#[tokio::test]
async fn input_arriving_during_compaction_is_fitted_into_the_immediate_request() {
    let (req_tx, mut requests) = mpsc::unbounded_channel();
    let wire = Arc::new(Controlled {
        replies: Mutex::new(VecDeque::from([futures::stream::iter(
            response("done").into_iter().map(Ok),
        )
        .boxed()])),
        requests: req_tx,
    });
    let mut agent = Agent::new(wire, common::spec());
    let compactor = Arc::new(Pausing {
        release: tokio::sync::Notify::new(),
        passes: std::sync::atomic::AtomicUsize::new(0),
    });
    agent.compactor = compactor.clone();
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut session = Session::with_prompt("original");
    let pending = Mutex::new(VecDeque::new());
    let input = || pending.lock().unwrap().pop_front();
    let (tx, _) = mpsc::unbounded_channel();
    let retry = Retry::default();
    let mut run = Box::pin(agent.run_with_input(&mut session, &ctx, &tx, &retry, &input));
    assert!(futures::poll!(run.as_mut()).is_pending());
    assert!(requests.try_recv().is_err());
    pending
        .lock()
        .unwrap()
        .push_back(prompt("arrived while compacting"));
    compactor.release.notify_one();
    let spent = run.await.unwrap();
    let req = requests.try_recv().unwrap();
    assert_eq!(
        req.messages.last().unwrap().text(),
        "arrived while compacting"
    );
    assert!(requests.try_recv().is_err(), "no stale request was sent");
    let passes = compactor.passes.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        passes > 1,
        "new input is checked against the context budget"
    );
    assert_eq!(
        spent.input,
        7 * passes as u64,
        "all fitting costs are retained"
    );
}

#[tokio::test]
async fn new_input_waits_for_the_whole_reply_then_continues_without_a_tool_call() {
    let (stream_tx, stream_rx) = mpsc::unbounded_channel();
    let first = futures::stream::unfold(stream_rx, |mut rx| async {
        rx.recv().await.map(|event| (Ok(event), rx))
    })
    .boxed();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let wire = Arc::new(Controlled {
        replies: Mutex::new(VecDeque::from([
            first,
            futures::stream::iter(response("updated").into_iter().map(Ok)).boxed(),
        ])),
        requests: req_tx,
    });
    let agent = Agent::new(wire, common::spec());
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut session = Session::with_prompt("original");
    let pending = Mutex::new(VecDeque::new());
    let input = || pending.lock().unwrap().pop_front();
    let (tx, mut events) = mpsc::unbounded_channel();
    let retry = Retry::default();
    let mut run = Box::pin(agent.run_with_input(&mut session, &ctx, &tx, &retry, &input));
    assert!(futures::poll!(run.as_mut()).is_pending());
    req_rx.try_recv().unwrap();
    stream_tx
        .send(StreamEvent::ReasoningDelta {
            index: 0,
            delta: "complete reasoning".into(),
        })
        .unwrap();
    assert!(futures::poll!(run.as_mut()).is_pending());
    pending.lock().unwrap().push_back(prompt("new direction"));
    assert!(futures::poll!(run.as_mut()).is_pending());
    assert!(
        req_rx.try_recv().is_err(),
        "input does not restart generation"
    );
    assert_eq!(pending.lock().unwrap().len(), 1);
    stream_tx
        .send(StreamEvent::ReasoningSignature {
            index: 0,
            signature: "intact".into(),
        })
        .unwrap();
    for mut event in response("original answer") {
        match &mut event {
            StreamEvent::BlockStart { index, .. } | StreamEvent::TextDelta { index, .. } => {
                *index = 1
            }
            _ => {}
        }
        stream_tx.send(event).unwrap();
    }
    drop(stream_tx);
    run.await.unwrap();
    let second = req_rx.try_recv().unwrap();
    assert!(req_rx.try_recv().is_err(), "exactly one continuation");
    assert_eq!(second.messages.last().unwrap().text(), "new direction");
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.text() == "original answer")
    );
    assert!(second.messages.iter().any(|m| matches!(m, Message::Assistant { content, .. }
        if content.iter().any(|b| matches!(b, AssistantContent::Reasoning(r)
            if r.text() == "complete reasoning"
                && matches!(r.replay_for(&common::spec()), llm::message::Replay::Thinking { signature: Some("intact") }))))));
    assert_eq!(
        session
            .entries()
            .iter()
            .filter(|e| matches!(e, Entry::Ask { ask, .. } if ask.text == "new direction"))
            .count(),
        1
    );
    let mut done = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, Event::Done { .. }) {
            done += 1;
        }
    }
    assert_eq!(done, 1, "the run ends after consuming input");
}

#[tokio::test]
async fn input_after_a_tool_call_follows_its_result_before_the_next_request() {
    let (req_tx, mut requests) = mpsc::unbounded_channel();
    let wire = Arc::new(Controlled {
        replies: Mutex::new(VecDeque::from([
            futures::stream::iter(
                vec![
                    StreamEvent::BlockStart {
                        index: 0,
                        kind: BlockKind::ToolCall {
                            id: Some("read-1".into()),
                            name: "read".into(),
                        },
                    },
                    StreamEvent::ToolArgsDelta {
                        index: 0,
                        delta: r#"{"path":"data"}"#.into(),
                    },
                    StreamEvent::Done {
                        stop: StopReason::ToolUse,
                        usage: Usage::default(),
                    },
                ]
                .into_iter()
                .map(Ok),
            )
            .boxed(),
            futures::stream::iter(response("done").into_iter().map(Ok)).boxed(),
        ])),
        requests: req_tx,
    });
    let mut agent = Agent::new(wire, common::spec());
    Arc::make_mut(&mut agent.brief).registry = toolbox::builtin();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data"), "result").unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut session = Session::with_prompt("read data");
    let pending = Mutex::new(Some(prompt("new direction")));
    let input = || {
        ctx.viewed(&dir.path().join("data"))?;
        pending.lock().unwrap().take()
    };
    let (tx, _) = mpsc::unbounded_channel();
    agent
        .run_with_input(&mut session, &ctx, &tx, &Retry::default(), &input)
        .await
        .unwrap();
    requests.try_recv().unwrap();
    let second = requests.try_recv().unwrap();
    assert_eq!(second.messages.last().unwrap().text(), "new direction");
    let tool_at = session
        .entries()
        .iter()
        .position(|e| matches!(e, Entry::Tool { result, .. } if result.call == "read-1"))
        .unwrap();
    let ask_at = session
        .entries()
        .iter()
        .position(|e| matches!(e, Entry::Ask { ask, .. } if ask.text == "new direction"))
        .unwrap();
    assert!(
        tool_at < ask_at,
        "a message must not split a call and its result"
    );
}

// Notes filed once the read has run, as a lane's are while its run works.
struct Noting<'a> {
    ctx: &'a Ctx,
    read: std::path::PathBuf,
    notes: Mutex<VecDeque<agent::seams::Note>>,
}

impl agent::seams::Input for Noting<'_> {
    fn take(&self) -> Option<Prompt> {
        None
    }

    fn note(&self) -> Option<agent::seams::Note> {
        self.ctx.viewed(&self.read)?;
        self.notes.lock().unwrap().pop_front()
    }
}

#[tokio::test]
async fn notes_go_with_the_next_request_and_only_unshown_ones_are_said() {
    let (req_tx, mut requests) = mpsc::unbounded_channel();
    let wire = Arc::new(Controlled {
        replies: Mutex::new(VecDeque::from([
            futures::stream::iter(
                vec![
                    StreamEvent::BlockStart {
                        index: 0,
                        kind: BlockKind::ToolCall {
                            id: Some("read-1".into()),
                            name: "read".into(),
                        },
                    },
                    StreamEvent::ToolArgsDelta {
                        index: 0,
                        delta: r#"{"path":"data"}"#.into(),
                    },
                    StreamEvent::Done {
                        stop: StopReason::ToolUse,
                        usage: Usage::default(),
                    },
                ]
                .into_iter()
                .map(Ok),
            )
            .boxed(),
            futures::stream::iter(response("done").into_iter().map(Ok)).boxed(),
        ])),
        requests: req_tx,
    });
    let mut agent = Agent::new(wire, common::spec());
    Arc::make_mut(&mut agent.brief).registry = toolbox::builtin();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data"), "result").unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut session = Session::with_prompt("read data");
    let input = Noting {
        ctx: &ctx,
        read: dir.path().join("data"),
        notes: Mutex::new(VecDeque::from([
            agent::seams::Note {
                text: "drawn already".into(),
                shown: true,
            },
            agent::seams::Note {
                text: "not yet drawn".into(),
                shown: false,
            },
        ])),
    };
    let (tx, mut events) = mpsc::unbounded_channel();
    agent
        .run_with_input(&mut session, &ctx, &tx, &Retry::default(), &input)
        .await
        .unwrap();
    requests.try_recv().unwrap();
    let second = requests.try_recv().unwrap();
    let sent: Vec<String> = second
        .messages
        .iter()
        .rev()
        .take(2)
        .map(|m| m.text())
        .collect();
    assert_eq!(
        sent,
        ["not yet drawn", "drawn already"],
        "after the result, in order"
    );
    let notes: Vec<_> = session
        .entries()
        .iter()
        .filter(|e| matches!(e, Entry::Note { .. }))
        .collect();
    assert_eq!(notes.len(), 2, "both are kept, once each");
    let mut said = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::Committed { entries } = event {
            said.extend(
                entries
                    .into_iter()
                    .filter(|e| matches!(e, Entry::Note { .. })),
            );
        }
    }
    assert_eq!(said.len(), 1, "a shown note is not said again");
    assert!(matches!(&said[0], Entry::Note { note, .. } if note == "not yet drawn"));
}

#[tokio::test]
async fn cancelling_a_run_keeps_unconsumed_input() {
    let (req_tx, _) = mpsc::unbounded_channel();
    let agent = Agent::new(
        Arc::new(Controlled {
            replies: Mutex::default(),
            requests: req_tx,
        }),
        common::spec(),
    );
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    ctx.cancel.cancel();
    let mut session = Session::with_prompt("go");
    let pending = Mutex::new(VecDeque::from([prompt("keep this")]));
    let input = || pending.lock().unwrap().pop_front();
    let (tx, _) = mpsc::unbounded_channel();
    assert!(matches!(
        agent
            .run_with_input(&mut session, &ctx, &tx, &Retry::default(), &input)
            .await,
        Err(AgentError::Cancelled)
    ));
    assert_eq!(pending.lock().unwrap().len(), 1);
}
