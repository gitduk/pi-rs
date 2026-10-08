use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use llm::message::{Message, UserContent};
use llm::model::ModelSpec;
use llm::request::Request;
use llm::stream::{BlockKind, StopReason, StreamEvent, Usage};
use llm::transport::Transport;
use serde_json::{Value, json};
use tokio::sync::mpsc;

mod common;
use common::spec;

use agent::session::Session;
use agent::{Agent, AgentError, Briefing, Ceiling, Event, Retry};
use tool::{Concurrency, Ctx, Registry, Tier, Tool, ToolError, ToolOutput, Workspace};

// Replays one scripted event list per turn, so the loop is exercised without
// a network.
struct Scripted {
    turns: Vec<Vec<StreamEvent>>,
    next: AtomicUsize,
}

impl Scripted {
    fn new(turns: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            turns,
            next: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Transport for Scripted {
    async fn stream(
        &self,
        _spec: &ModelSpec,
        _req: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        let events = self.turns.get(i).cloned().unwrap_or_default();
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

fn text_turn(body: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            delta: body.into(),
        },
        StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: Usage {
                input: 3_000,
                output: 5,
                ..Default::default()
            },
        },
    ]
}

fn call_turn(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
    let mut ev = Vec::new();
    for (i, (id, name, args)) in calls.iter().enumerate() {
        ev.push(StreamEvent::BlockStart {
            index: i,
            kind: BlockKind::ToolCall {
                id: Some((*id).into()),
                name: (*name).into(),
            },
        });
        ev.push(StreamEvent::ToolArgsDelta {
            index: i,
            delta: (*args).into(),
        });
    }
    ev.push(StreamEvent::Done {
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    });
    ev
}

fn harness(turns: Vec<Vec<StreamEvent>>) -> (tempfile::TempDir, Agent, Ctx) {
    let (dir, agent, ctx, _) = wired(turns);
    (dir, agent, ctx)
}

// The same, keeping the wire so a test can read what was sent to it.
fn wired(turns: Vec<Vec<StreamEvent>>) -> (tempfile::TempDir, Agent, Ctx, Arc<Scripted>) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let wire = Scripted::new(turns);
    let agent = tooled(wire.clone(), spec());
    (dir, agent, Ctx::new(ws), wire)
}

// A run with the tools a real one carries: `Agent::new` starts with none.
fn tooled(transport: Arc<dyn Transport>, spec: ModelSpec) -> Agent {
    let mut a = Agent::new(transport, spec);
    brief(&mut a).registry = toolbox::builtin();
    a
}

// The brief is one value behind an `Arc`; a test that rewrites part of it takes
// the copy the same way the CLI does.
fn brief(a: &mut Agent) -> &mut Briefing {
    std::sync::Arc::make_mut(&mut a.brief)
}

async fn drive(
    agent: &Agent,
    ctx: &Ctx,
    prompt: &str,
) -> (Session, Result<llm::stream::Usage, AgentError>, Vec<Event>) {
    drive_retrying(agent, ctx, prompt, &Retry::default()).await
}

// The same, on a schedule the test decides.
async fn drive_retrying(
    agent: &Agent,
    ctx: &Ctx,
    prompt: &str,
    retry: &Retry,
) -> (Session, Result<llm::stream::Usage, AgentError>, Vec<Event>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut session = Session::with_prompt(prompt);
    let out = agent.run(&mut session, ctx, &tx, retry).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    (session, out, events)
}

// Every result in the view, in order: one entry is one message, spread
// across several rather than packed into one — joining is the wire's business.
fn tool_results(msgs: &[Message]) -> Vec<&llm::message::ToolResult> {
    msgs.iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|c| match c {
            UserContent::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn every_call_gets_a_result_even_when_nothing_runs() {
    let (_d, a, ctx) = harness(vec![
        call_turn(&[
            ("t1", "read", r#"{"path":"missing.txt"}"#),
            ("t2", "nosuchtool", "{}"),
            ("t3", "read", r#"{"path": "#),
        ]),
        text_turn("ok"),
    ]);
    let (session, out, _) = drive(&a, &ctx, "go").await;
    out.unwrap();

    // An unanswered tool_use makes the next request invalid on both wires.
    let view = session.context();
    let results = tool_results(&view);
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.is_error), "{results:?}");
    assert!(
        results[1].flatten_text().contains("no tool named"),
        "{:?}",
        results[1]
    );
    assert!(
        results[2].flatten_text().contains("not valid JSON"),
        "{:?}",
        results[2]
    );
    // The model's own text rides back in the rejection, or it can only guess
    // at what failed to parse from the column number.
    assert!(
        results[2].flatten_text().contains(r#"you sent: {"path": "#),
        "{:?}",
        results[2]
    );

    let calls: Vec<_> = view[1].tool_calls().collect();
    assert_eq!(calls.len(), 3);
    for (call, result) in calls.iter().zip(&results) {
        assert_eq!(call.id, result.call, "results must line up with calls");
    }
}

#[tokio::test]
async fn a_denied_tier_comes_back_as_a_result_not_an_abort() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[("t1", "bash", r#"{"command":"echo hi"}"#)]),
        text_turn("understood"),
    ]);
    brief(&mut a).approver = Arc::new(Ceiling(Tier::Read));
    let (session, out, events) = drive(&a, &ctx, "run it").await;
    out.unwrap();

    let view = session.context();
    let results = tool_results(&view);
    assert!(results[0].is_error);
    assert!(
        results[0].flatten_text().contains("capped at Read"),
        "{:?}",
        results[0]
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolDenied { name, .. } if name == "bash"))
    );
}

// Finishes after `delay_ms`, reporting its own name.
struct Sleeper {
    name: &'static str,
    delay_ms: u64,
    exclusive: bool,
}

#[async_trait]
impl Tool for Sleeper {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "test"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn concurrency(&self) -> Concurrency {
        if self.exclusive {
            Concurrency::Exclusive
        } else {
            Concurrency::Shared
        }
    }
    async fn execute(&self, _args: Value, _ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(ToolOutput::text(self.name))
    }
}

#[tokio::test]
async fn parallel_results_follow_call_order_not_completion_order() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[("t1", "slow", "{}"), ("t2", "fast", "{}")]),
        text_turn("ok"),
    ]);
    brief(&mut a).registry = Registry::new()
        .with(Sleeper {
            name: "slow",
            delay_ms: 120,
            exclusive: false,
        })
        .with(Sleeper {
            name: "fast",
            delay_ms: 120,
            exclusive: false,
        });

    let started = std::time::Instant::now();
    let (session, out, _) = drive(&a, &ctx, "go").await;
    out.unwrap();

    let view = session.context();
    let results = tool_results(&view);
    assert_eq!(
        results[0].flatten_text(),
        "slow",
        "the slow call was issued first"
    );
    assert_eq!(results[1].flatten_text(), "fast");
    // Two 120ms calls that overlap land near 120ms, not 240ms; the headroom
    // keeps a loaded CI from flaking without masking a real regression.
    assert!(
        started.elapsed().as_millis() < 190,
        "shared calls must overlap"
    );
}

// A quick call in a batch says it is done when it is, not when the slowest
// is: the screen marks each call by its own end.
#[tokio::test]
async fn each_call_says_its_end_as_it_lands() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[("t1", "slow", "{}"), ("t2", "fast", "{}")]),
        text_turn("ok"),
    ]);
    brief(&mut a).registry = Registry::new()
        .with(Sleeper {
            name: "slow",
            delay_ms: 150,
            exclusive: false,
        })
        .with(Sleeper {
            name: "fast",
            delay_ms: 10,
            exclusive: false,
        });

    let (session, out, events) = drive(&a, &ctx, "go").await;
    out.unwrap();

    let ended: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolEnd { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ended, ["t2", "t1"], "the fast call ends first");
    let view = session.context();
    let results = tool_results(&view);
    assert_eq!(results[0].flatten_text(), "slow", "results keep call order");
    assert_eq!(results[1].flatten_text(), "fast");
}

#[tokio::test]
async fn an_exclusive_call_runs_alone_and_its_neighbours_still_join() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[
            ("t1", "solo", "{}"),
            ("t2", "slow", "{}"),
            ("t3", "fast", "{}"),
        ]),
        text_turn("ok"),
    ]);
    brief(&mut a).registry = Registry::new()
        .with(Sleeper {
            name: "solo",
            delay_ms: 80,
            exclusive: true,
        })
        .with(Sleeper {
            name: "slow",
            delay_ms: 80,
            exclusive: false,
        })
        .with(Sleeper {
            name: "fast",
            delay_ms: 10,
            exclusive: false,
        });

    let started = std::time::Instant::now();
    let (_s, out, events) = drive(&a, &ctx, "go").await;
    out.unwrap();
    assert!(
        started.elapsed().as_millis() >= 160,
        "an exclusive call must not overlap"
    );
    let ended: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolEnd { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ended, ["t1", "t3", "t2"], "the shared pair must overlap");
}

#[tokio::test]
async fn cancellation_stops_the_run() {
    let (_d, a, ctx) = harness(vec![
        call_turn(&[("t1", "read", r#"{"path":"x"}"#)]),
        text_turn("no"),
    ]);
    ctx.cancel.cancel();
    let (_s, out, _) = drive(&a, &ctx, "go").await;
    assert!(matches!(out, Err(AgentError::Cancelled)), "{out:?}");
}

// Stops the run from inside a call, the way a user's Esc lands mid-batch.
struct Halt;

#[async_trait]
impl Tool for Halt {
    fn name(&self) -> &str {
        "halt"
    }
    fn description(&self) -> &str {
        "test"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    async fn execute(&self, _args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        ctx.cancel.cancel();
        Err(ToolError::Cancelled)
    }
}

#[tokio::test]
async fn a_call_that_finished_beside_a_cancelled_one_keeps_its_result() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[("t1", "done", "{}"), ("t2", "halt", "{}")]),
        text_turn("no"),
    ]);
    brief(&mut a).registry = Registry::new()
        .with(Sleeper {
            name: "done",
            delay_ms: 0,
            exclusive: false,
        })
        .with(Halt);

    let (mut session, out, _) = drive(&a, &ctx, "go").await;
    assert!(matches!(out, Err(AgentError::Cancelled)), "{out:?}");

    session.send_prompt("next", None);
    let view = session.context();
    let results = tool_results(&view);
    let text_of = |id: &str| {
        results
            .iter()
            .find(|r| r.call == id)
            .map(|r| r.flatten_text())
    };
    assert_eq!(
        text_of("t1").as_deref(),
        Some("done"),
        "the finished call ran"
    );
    assert_eq!(
        text_of("t2").as_deref(),
        Some(agent::session::STOPPED_CALL),
        "only the cancelled call reads as stopped"
    );
}

#[tokio::test]
async fn cancellation_during_stream_saves_partial_assistant_response() {
    struct PartialStream {
        cancel: tokio_util::sync::CancellationToken,
    }

    #[async_trait]
    impl Transport for PartialStream {
        async fn stream(
            &self,
            _spec: &ModelSpec,
            _req: &Request,
        ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
            let cancel = self.cancel.clone();
            let stream = futures::stream::unfold((0usize, cancel), |(step, cancel)| async move {
                match step {
                    0 => Some((
                        Ok(StreamEvent::BlockStart {
                            index: 0,
                            kind: BlockKind::Text,
                        }),
                        (1, cancel),
                    )),
                    1 => Some((
                        Ok(StreamEvent::TextDelta {
                            index: 0,
                            delta: "partial output".into(),
                        }),
                        (2, cancel),
                    )),
                    _ => {
                        cancel.cancel();
                        futures::future::pending::<()>().await;
                        None
                    }
                }
            });
            Ok(stream.boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let ctx = Ctx::new(ws);
    let wire = Arc::new(PartialStream {
        cancel: ctx.cancel.clone(),
    });
    let agent = tooled(wire, spec());

    let (mut session, out, _) = drive(&agent, &ctx, "first prompt").await;
    assert!(matches!(out, Err(AgentError::Cancelled)), "{out:?}");

    // The partial stream was saved as an assistant answer
    let entries = session.entries();
    assert_eq!(entries.len(), 2, "must have ask and partial answer");
    assert!(matches!(&entries[0], agent::session::Entry::Ask { .. }));
    let answer = match &entries[1] {
        agent::session::Entry::Answer { blocks, .. } => blocks,
        other => panic!("expected answer entry, got {other:?}"),
    };
    assert_eq!(answer.len(), 1);
    match &answer[0] {
        llm::message::AssistantContent::Text(t) => assert_eq!(t.text, "partial output"),
        other => panic!("expected text block, got {other:?}"),
    }

    // A subsequent prompt does not produce consecutive ask entries
    session.send_prompt("second prompt", None);
    let msgs = session.context();
    assert_eq!(msgs.len(), 3, "user -> assistant -> user");
    assert!(matches!(msgs[0], llm::message::Message::User { .. }));
    assert!(matches!(msgs[1], llm::message::Message::Assistant { .. }));
    assert!(matches!(msgs[2], llm::message::Message::User { .. }));
}

// Answers tool-bearing turns from a script and any tool-free turn — which is
// what a summarization request is — with a fixed summary.
struct WithSummarizer {
    turns: Vec<Vec<StreamEvent>>,
    next: AtomicUsize,
    summaries: AtomicUsize,
}

#[async_trait]
impl Transport for WithSummarizer {
    async fn stream(
        &self,
        _spec: &ModelSpec,
        req: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        let events = if req.tools.is_empty() {
            self.summaries.fetch_add(1, Ordering::SeqCst);
            // The history to summarize arrives flattened into one user turn.
            assert_eq!(req.messages.len(), 1, "a summarization request is one turn");
            assert!(
                req.messages[0].text().contains("[calls read]"),
                "{:?}",
                req.messages[0]
            );
            text_turn("read a.txt twice; nothing changed on disk")
        } else {
            let i = self.next.fetch_add(1, Ordering::SeqCst);
            self.turns
                .get(i)
                .cloned()
                .unwrap_or_else(|| text_turn("done"))
        };
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

// Weight in assistant prose, which omission cannot reclaim — only dropping the
// exchange does, and that is what the summarizer exists for.
fn bulky_turn(id: &str) -> Vec<StreamEvent> {
    let mut ev = vec![
        StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            delta: format!("{id}: ") + &"w".repeat(20_000),
        },
        StreamEvent::BlockStart {
            index: 1,
            kind: BlockKind::ToolCall {
                id: Some("c1".into()),
                name: "read".into(),
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 1,
            delta: r#"{"path":"a.txt"}"#.into(),
        },
    ];
    ev.push(StreamEvent::Done {
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    });
    ev
}

// A cheap summarizer's tokens must be billed at the run's rate, not its own —
// otherwise a total that looks plausible is silently wrong.
#[tokio::test]
async fn a_summary_on_another_model_still_counts_toward_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "z".repeat(2_000)).unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    // One each: the transport counts its own turns, so sharing one would make
    // the second run answer from where the first stopped.
    let wire = || {
        Arc::new(WithSummarizer {
            turns: (0..4).map(|i| bulky_turn(&format!("t{i}"))).collect(),
            next: AtomicUsize::new(0),
            summaries: AtomicUsize::new(0),
        })
    };

    let mut main = spec();
    main.context_window = 24_000;
    main.max_output_tokens = 2_000;

    // Same endpoint, a different model: what it costs is the surface's
    // arithmetic now, but whose tokens they are is still the loop's business.
    let mut cheap = main.clone();
    cheap.model = "cheap".into();

    let delegated = wire();
    let mut a = tooled(delegated.clone(), main.clone());
    common::compacting(&mut a, Some((wire(), cheap)));
    let cheaply = drive(&a, &ctx, "read it repeatedly").await.1.unwrap();

    let itself = wire();
    let mut b = tooled(itself.clone(), main);
    // No summarizer of its own: the summary is written by the model doing the
    // work, which is the thing under test.
    common::compacting(&mut b, None);
    let dearly = drive(&b, &ctx, "read it repeatedly").await.1.unwrap();

    assert!(
        itself.summaries.load(Ordering::SeqCst) > 0,
        "nothing summarized"
    );
    assert!(
        cheaply.input > 0 && cheaply.input == dearly.input,
        "the summary's tokens belong to the run that paid for them: {cheaply:?} vs {dearly:?}"
    );
}

#[tokio::test]
async fn dropped_history_comes_back_as_a_summary_on_the_opening_turn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "z".repeat(2_000)).unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let ctx = Ctx::new(ws);

    let transport = Arc::new(WithSummarizer {
        turns: vec![
            bulky_turn("t1"),
            bulky_turn("t2"),
            bulky_turn("t3"),
            bulky_turn("t4"),
        ],
        next: AtomicUsize::new(0),
        summaries: AtomicUsize::new(0),
    });
    let mut spec = spec();
    spec.context_window = 24_000;
    spec.max_output_tokens = 2_000;

    let mut a = tooled(transport.clone(), spec);
    common::compacting(&mut a, None);

    let (session, out, events) = drive(&a, &ctx, "read it repeatedly").await;
    out.unwrap();

    assert!(
        transport.summaries.load(Ordering::SeqCst) > 0,
        "the summarizer must have run"
    );
    let compacted: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Compacted(r) => Some(*r),
            _ => None,
        })
        .collect();
    assert!(compacted.iter().any(|r| r.summarized), "{compacted:?}");

    // The summary rides the opening user turn, so the roles still alternate.
    let view = session.context();
    assert!(view[0].text().contains("<earlier-work>"), "{:?}", view[0]);
    assert!(view[0].text().contains("read a.txt twice"), "{:?}", view[0]);
    assert!(
        matches!(view[1], Message::Assistant { .. }),
        "{:?}",
        view[1]
    );

    // And the bodies it replaced are still in the log.
    assert!(session.entries().len() > view.len());
}

#[tokio::test]
async fn a_summarizer_that_fails_drops_the_history_without_failing_the_turn() {
    struct Broken(AtomicUsize);

    #[async_trait]
    impl Transport for Broken {
        async fn stream(
            &self,
            _: &ModelSpec,
            req: &Request,
        ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
            if req.tools.is_empty() {
                return Err(llm::LlmError::Stream("summarizer is down".into()));
            }
            let i = self.0.fetch_add(1, Ordering::SeqCst);
            let events = if i < 4 {
                bulky_turn(&format!("t{i}"))
            } else {
                text_turn("done")
            };
            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "z".repeat(2_000)).unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    let mut spec = spec();
    spec.context_window = 24_000;
    spec.max_output_tokens = 2_000;
    let mut a = tooled(Arc::new(Broken(AtomicUsize::new(0))), spec);
    common::compacting(&mut a, None);

    let (_session, out, events) = drive(&a, &ctx, "read it repeatedly").await;
    // Losing the summary costs context; failing the turn costs the whole run.
    out.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Compacted(r) if r.dropped > 0 && !r.summarized))
    );
}

// Fails the first `fail` attempts with `err`, then answers normally.
struct Flaky {
    remaining: AtomicUsize,
    err: fn() -> llm::LlmError,
}

#[async_trait]
impl Transport for Flaky {
    async fn stream(
        &self,
        _: &ModelSpec,
        _: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) > 0 {
            return Err((self.err)());
        }
        Ok(futures::stream::iter(text_turn("recovered").into_iter().map(Ok)).boxed())
    }
}

fn flaky(times: usize, err: fn() -> llm::LlmError) -> Arc<Flaky> {
    Arc::new(Flaky {
        remaining: AtomicUsize::new(times),
        err,
    })
}

// Short enough that a test's retries land inside the test.
fn fast_retry() -> Retry {
    Retry {
        base: std::time::Duration::from_millis(1),
        max: std::time::Duration::from_millis(4),
        ..Retry::default()
    }
}

#[tokio::test]
async fn a_throttled_request_is_retried_until_it_lands() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let a = tooled(
        flaky(2, || llm::LlmError::Api {
            format: "anthropic",
            status: 429,
            body: "rate limit exceeded".into(),
        }),
        spec(),
    );
    let retry = fast_retry();

    let (session, out, events) = drive_retrying(&a, &ctx, "go", &retry).await;
    out.unwrap();
    assert_eq!(session.context()[1].text(), "recovered");

    let retries: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::Retrying { .. }))
        .collect();
    assert_eq!(retries.len(), 2, "{retries:?}");
}

// Retrying is bounded by the attempt budget, whatever the failure reads as:
// transient-looking or not, the run gives up rather than hammering forever.
#[tokio::test]
async fn retries_stop_at_the_attempt_budget() {
    // A case: what it is called, the failure to answer with, and how many
    // attempts that should take.
    type Case = (&'static str, fn() -> llm::LlmError, usize);
    let cases: &[Case] = &[
        (
            "429",
            || llm::LlmError::Api {
                format: "anthropic",
                status: 429,
                body: "rate limit exceeded".into(),
            },
            Retry::default().attempts,
        ),
        (
            "stream",
            || llm::LlmError::Stream("connection reset".into()),
            3,
        ),
    ];
    for (kind, err, attempts) in cases {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
        let a = tooled(flaky(99, *err), spec());
        let mut retry = fast_retry();
        retry.attempts = *attempts;

        let (_s, out, events) = drive_retrying(&a, &ctx, "go", &retry).await;
        assert!(out.is_err(), "{kind}: {out:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Retrying { .. }))
                .count(),
            *attempts,
            "{kind}: retries stop at the budget"
        );
    }
}

// Opens a stream and then never sends anything.
struct Wedged;

#[async_trait]
impl Transport for Wedged {
    async fn stream(
        &self,
        _: &ModelSpec,
        _: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        Ok(futures::stream::pending().boxed())
    }
}

#[tokio::test]
async fn a_stream_that_stops_sending_does_not_hold_the_turn_open() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let a = tooled(Arc::new(Wedged), spec());
    let mut retry = fast_retry();
    retry.attempts = 1;
    retry.idle = std::time::Duration::from_millis(120);

    let started = std::time::Instant::now();
    let (_s, out, events) = drive_retrying(&a, &ctx, "go", &retry).await;

    assert!(out.is_err(), "{out:?}");
    assert!(
        started.elapsed().as_secs() < 5,
        "the watchdog must fire, not the test"
    );
    // A wedged stream reads as transient, so it is retried before giving up.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Retrying { .. }))
            .count(),
        1
    );
}

fn call_message(id: &str) -> Message {
    Message::Assistant {
        content: vec![llm::message::AssistantContent::ToolCall(
            llm::message::ToolCall {
                id: id.into(),
                name: "read".into(),
                args: json!({ "path": "a.rs" }),
            },
        )],
    }
}

// Refuses oversized requests until the transcript shrinks below `fits`.
// `named` is the window the refusal states, when it states one.
struct Picky {
    fits: usize,
    named: Option<usize>,
    refusals: AtomicUsize,
}

#[async_trait]
impl Transport for Picky {
    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        let size = llm::estimate::tokens(&req.messages, spec);
        if size > self.fits {
            self.refusals.fetch_add(1, Ordering::SeqCst);
            // 413, not 400: only transient statuses are retried after a squeeze.
            let body = match self.named {
                // A provider counts more than our estimate does, or it would
                // not be refusing a request we fitted to its window.
                Some(limit) => format!(
                    "prompt is too long: {} tokens > {limit} maximum",
                    size + limit
                ),
                None => "Request exceeds the maximum size".into(),
            };
            return Err(llm::LlmError::Api {
                format: "anthropic",
                status: 413,
                body,
            });
        }
        Ok(futures::stream::iter(text_turn("fits now").into_iter().map(Ok)).boxed())
    }
}

// A transcript our own estimate calls comfortable: three fat tool results the
// compaction can actually shrink, unlike a lone huge prompt.
fn fat_history() -> Vec<Message> {
    let mut history = vec![Message::user("the task")];
    for i in 0..3 {
        history.push(call_message(&format!("h{i}")));
        history.push(Message::tool_results(vec![llm::message::ToolResult::text(
            format!("h{i}"),
            "read",
            "z".repeat(12_000),
        )]));
    }
    history
}

// An overflow refusal squeezes the transcript and retries — refitting to the
// window when the refusal names one, squeezing blindly when it does not.
#[tokio::test]
async fn an_overflow_refusal_shrinks_the_transcript_and_retries() {
    // The named row keeps the roomy default window, testing the refit itself;
    // the unnamed row squeezes blindly, the realistic off-by-a-third case.
    for (window, fits, named) in [
        (200_000u32, 2_000usize, Some(2_000usize)),
        (60_000, 8_000, None),
        // The window the run was already fitted to: refitting alone would
        // resend the same request until the squeezes ran out.
        (60_000, 8_000, Some(60_000)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
        let picky = Arc::new(Picky {
            fits,
            named,
            refusals: AtomicUsize::new(0),
        });

        let mut a = tooled(picky.clone(), spec());
        common::compacting(&mut a, None);
        let retry = fast_retry();
        std::sync::Arc::make_mut(&mut a.model).spec.context_window = window;
        let mut session = Session::from_messages(fat_history());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = a.run(&mut session, &ctx, &tx, &retry).await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }

        out.unwrap();
        assert!(
            picky.refusals.load(Ordering::SeqCst) > 0,
            "the refusal must have happened"
        );
        match named {
            Some(limit) if limit < window as usize => {
                // Refitted to the named window rather than squeezed blindly,
                // so compaction lands inside what the provider measured.
                assert!(
                    events
                        .iter()
                        .any(|e| matches!(e, Event::Warning(w) if w.contains("2000-token window"))),
                    "{events:?}"
                );
                assert!(
                    events
                        .iter()
                        .any(|e| matches!(e, Event::Compacted(r) if r.after < 1_000)),
                    "{events:?}"
                );
            }
            _ => assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Event::Warning(w) if w.contains("named no limit"))),
                "{events:?}"
            ),
        }
        assert_eq!(session.context().last().unwrap().text(), "fits now");
    }
}

// Records each request's estimated input and its output cap.
#[derive(Default)]
struct Sizes(std::sync::Mutex<Vec<(usize, Option<u32>)>>);

#[async_trait]
impl Transport for Sizes {
    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        let input = llm::estimate::tokens(&req.messages, spec)
            + req.system_text().map_or(0, llm::estimate::text)
            + llm::estimate::tool_defs(&req.tools);
        self.0.lock().unwrap().push((input, req.max_output_tokens));
        Ok(futures::stream::iter(text_turn("ok").into_iter().map(Ok)).boxed())
    }
}

// Anthropic refuses input + max_tokens past the window, so the cap sent must
// be the reply the budget reserved, not the spec's larger one.
#[tokio::test]
async fn input_and_output_cap_fit_the_window_together() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let sizes = Arc::new(Sizes::default());
    let mut spec = spec();
    spec.context_window = 200_000;
    spec.max_output_tokens = 120_000;
    let mut a = tooled(sizes.clone(), spec);
    common::compacting(&mut a, None);
    // Inside the budget, yet past the window once the spec's cap is added.
    let mut history = vec![Message::user("the task")];
    for i in 0..3 {
        history.push(call_message(&format!("h{i}")));
        history.push(Message::tool_results(vec![llm::message::ToolResult::text(
            format!("h{i}"),
            "read",
            "z".repeat(120_000),
        )]));
    }
    let mut session = Session::from_messages(history);
    let (tx, _rx) = mpsc::unbounded_channel();
    a.run(&mut session, &ctx, &tx, &fast_retry()).await.unwrap();

    let sizes = sizes.0.lock().unwrap();
    assert!(!sizes.is_empty());
    for &(input, cap) in sizes.iter() {
        let cap = cap.map_or(120_000, |c| c as usize);
        assert!(input + cap <= 200_000, "{input} + {cap} > 200000");
    }
}

// Refuses three ways in order — unnamed, named, unnamed — so what the second
// blind squeeze is measured against becomes visible in the warnings.
struct Mixed {
    calls: AtomicUsize,
    limit: usize,
}

#[async_trait]
impl Transport for Mixed {
    async fn stream(
        &self,
        _spec: &ModelSpec,
        _req: &Request,
    ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
        let unnamed = || llm::LlmError::Api {
            format: "anthropic",
            status: 413,
            body: "Request exceeds the maximum size".into(),
        };
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(unnamed()),
            // 413, not 400: only transient statuses are retried after a squeeze.
            1 => Err(llm::LlmError::Api {
                format: "anthropic",
                status: 413,
                body: format!("prompt is too long: 99999 tokens > {} maximum", self.limit),
            }),
            2 => Err(unnamed()),
            _ => Ok(futures::stream::iter(text_turn("fits now").into_iter().map(Ok)).boxed()),
        }
    }
}

// A blind squeeze shrinks the estimate the spec claimed; once the provider
// names a real window, that baseline — and the discount with it — is gone.
#[tokio::test]
async fn a_named_window_supersedes_the_guesswork_that_preceded_it() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    let a = tooled(
        Arc::new(Mixed {
            calls: AtomicUsize::new(0),
            limit: 40_000,
        }),
        spec(),
    );
    let retry = fast_retry();
    let mut session = Session::from_messages(vec![Message::user("the task")]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out = a.run(&mut session, &ctx, &tx, &retry).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    out.unwrap();

    let blind: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Warning(w) if w.contains("named no limit") => Some(w),
            _ => None,
        })
        .collect();
    assert_eq!(blind.len(), 2, "{events:?}");
    // Both are the first squeeze against their own baseline. Compounding them
    // would print 36% here, against a window the provider measured.
    assert!(blind[1].contains("60%"), "{}", blind[1]);
}

// Fails every call with a coded timeout, so the loop's code plumbing can be
// observed end to end.
struct Timeouter;

#[async_trait]
impl Tool for Timeouter {
    fn name(&self) -> &str {
        "timeouter"
    }

    fn description(&self) -> &str {
        "test"
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, _args: Value, _ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Timeout { ms: 42 })
    }
}

#[tokio::test]
async fn a_coded_tool_error_reaches_the_model_with_its_code() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[("t1", "timeouter", "{}")]),
        text_turn("ok"),
    ]);
    brief(&mut a).registry = Registry::new().with(Timeouter);

    let (session, out, _) = drive(&a, &ctx, "go").await;
    out.unwrap();
    let view = session.context();
    let results = tool_results(&view);
    assert!(results[0].is_error);
    let body = results[0].flatten_text();
    assert!(body.starts_with("Error: timed out after 42ms"), "{body}");
    assert!(body.ends_with("[code: TOOL_TIMEOUT]"), "{body}");
}
