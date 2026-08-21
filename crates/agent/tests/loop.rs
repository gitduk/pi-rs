use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use brain::catalog::{
    AnthropicCompat, Capabilities, ModelSpec, Pricing, ThinkingReplay, ThinkingSupport, Wire,
};
use brain::message::{AssistantContent, Message, ProviderCallId, UserContent};
use brain::request::Request;
use brain::stream::{BlockKind, StopReason, StreamEvent, Usage};
use brain::transport::Transport;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use agent::{Agent, AgentError, Ceiling, Event, Session};
use tools::{Concurrency, Ctx, Registry, Tier, Tool, ToolError, ToolOutput, Workspace};

/// Replays one scripted event list per turn, so the loop is exercised without
/// a network.
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
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn stream(
        &self,
        _spec: &ModelSpec,
        _req: &Request,
    ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
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
                input: 10,
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
                provider: Some(ProviderCallId((*id).into())),
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

fn spec() -> ModelSpec {
    ModelSpec {
        id: "test".into(),
        wire_id: "claude-test".into(),
        base_url: "http://localhost".into(),
        wire: Wire::Anthropic(AnthropicCompat::default()),
        context_window: 200_000,
        max_output_tokens: 8_000,
        caps: Capabilities {
            tools: true,
            parallel_tool_calls: true,
            vision: true,
            thinking: Some(ThinkingSupport::Budget),
            cache_breakpoints: true,
        },
        thinking_replay: ThinkingReplay::Signed,
        pricing: Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            ..Default::default()
        },
    }
}

fn harness(turns: Vec<Vec<StreamEvent>>) -> (tempfile::TempDir, Agent, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let agent = Agent::new(Scripted::new(turns), spec());
    (dir, agent, Ctx::new(ws))
}

async fn drive(
    agent: &Agent,
    ctx: &Ctx,
    prompt: &str,
) -> (Session, Result<agent::Totals, AgentError>, Vec<Event>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut session = Session::with_prompt(prompt);
    let out = agent.run(&mut session, ctx, &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    (session, out, events)
}

fn tool_results(msg: &Message) -> Vec<&brain::message::ToolResult> {
    match msg {
        Message::User { content } => content
            .iter()
            .filter_map(|c| match c {
                UserContent::ToolResult(r) => Some(r),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

#[tokio::test]
async fn a_turn_without_tool_calls_ends_the_run() {
    let (_d, a, ctx) = harness(vec![text_turn("done")]);
    let (session, out, events) = drive(&a, &ctx, "hi").await;

    let totals = out.unwrap();
    assert_eq!(totals.usage.input, 10);
    assert_eq!(totals.cost, 10.0 / 1e6 + 5.0 * 2.0 / 1e6);
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[1].text(), "done");
    assert!(events.contains(&Event::TextDelta("done".into())));
}

#[tokio::test]
async fn a_tool_call_round_trips_into_the_transcript() {
    let (_d, a, ctx) = harness(vec![
        call_turn(&[("t1", "write", r#"{"path":"a.txt","content":"hello\n"}"#)]),
        text_turn("wrote it"),
    ]);
    let (session, out, _) = drive(&a, &ctx, "make a.txt").await;
    out.unwrap();

    // user, assistant(call), user(result), assistant(text)
    assert_eq!(session.messages.len(), 4);
    let results = tool_results(&session.messages[2]);
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error, "{:?}", results[0]);
    assert_eq!(results[0].name, "write");
    assert_eq!(
        std::fs::read_to_string(ctx.workspace.root().join("a.txt")).unwrap(),
        "hello\n"
    );
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
    let results = tool_results(&session.messages[2]);
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

    let calls: Vec<_> = session.messages[1].tool_calls().collect();
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
    a.approver = Arc::new(Ceiling(Tier::Read));
    let (session, out, events) = drive(&a, &ctx, "run it").await;
    out.unwrap();

    let results = tool_results(&session.messages[2]);
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

/// Finishes after `delay_ms`, reporting its own name.
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
    a.registry = Registry::new()
        .with(Sleeper {
            name: "slow",
            delay_ms: 120,
            exclusive: false,
        })
        .with(Sleeper {
            name: "fast",
            delay_ms: 0,
            exclusive: false,
        });

    let started = std::time::Instant::now();
    let (session, out, _) = drive(&a, &ctx, "go").await;
    out.unwrap();

    let results = tool_results(&session.messages[2]);
    assert_eq!(
        results[0].flatten_text(),
        "slow",
        "the slow call was issued first"
    );
    assert_eq!(results[1].flatten_text(), "fast");
    assert!(
        started.elapsed().as_millis() < 240,
        "shared calls must overlap"
    );
}

#[tokio::test]
async fn an_exclusive_call_forces_the_batch_to_run_serially() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[("t1", "solo", "{}"), ("t2", "other", "{}")]),
        text_turn("ok"),
    ]);
    a.registry = Registry::new()
        .with(Sleeper {
            name: "solo",
            delay_ms: 80,
            exclusive: true,
        })
        .with(Sleeper {
            name: "other",
            delay_ms: 80,
            exclusive: false,
        });

    let started = std::time::Instant::now();
    let (_s, out, _) = drive(&a, &ctx, "go").await;
    out.unwrap();
    assert!(
        started.elapsed().as_millis() >= 160,
        "an exclusive call must not overlap"
    );
}

#[tokio::test]
async fn the_turn_limit_stops_a_runaway_loop_but_keeps_the_transcript() {
    let (_d, mut a, ctx) = harness(vec![
        call_turn(&[("t1", "read", r#"{"path":"x"}"#)]),
        call_turn(&[("t2", "read", r#"{"path":"x"}"#)]),
        call_turn(&[("t3", "read", r#"{"path":"x"}"#)]),
    ]);
    a.max_turns = 2;
    let (session, out, _) = drive(&a, &ctx, "loop forever").await;

    assert!(matches!(out, Err(AgentError::TurnLimit(2))), "{out:?}");
    assert_eq!(
        session.messages.len(),
        5,
        "work done before the limit survives"
    );
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

#[tokio::test]
async fn reasoning_deltas_reach_the_renderer_separately_from_text() {
    let (_d, a, ctx) = harness(vec![vec![
        StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::Reasoning,
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            delta: "thinking".into(),
        },
        StreamEvent::BlockStart {
            index: 1,
            kind: BlockKind::Text,
        },
        StreamEvent::TextDelta {
            index: 1,
            delta: "answer".into(),
        },
        StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]]);
    let (session, out, events) = drive(&a, &ctx, "hi").await;
    out.unwrap();

    assert!(events.contains(&Event::ReasoningDelta("thinking".into())));
    assert!(events.contains(&Event::TextDelta("answer".into())));
    let Message::Assistant { content, .. } = &session.messages[1] else {
        panic!()
    };
    assert!(matches!(content[0], AssistantContent::Reasoning(_)));
}
