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
        wire_id: "test-wire-id".into(),
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
) -> (Session, Result<agent::Outcome, AgentError>, Vec<Event>) {
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

    let totals = out.unwrap().totals;
    assert_eq!(totals.usage.input, 3_000);
    assert_eq!(totals.cost, 3_000.0 / 1e6 + 5.0 * 2.0 / 1e6);
    assert_eq!(session.context().len(), 2);
    assert_eq!(session.context()[1].text(), "done");
    assert!(events.contains(&Event::TextDelta("done".into())));
}

/// A turn whose Done carries exactly this usage.
fn turn_reporting(body: &str, usage: Usage) -> Vec<StreamEvent> {
    let mut ev = text_turn(body);
    ev.pop();
    ev.push(StreamEvent::Done {
        stop: StopReason::EndTurn,
        usage,
    });
    ev
}

#[tokio::test]
async fn a_provider_that_reports_nothing_gets_our_own_count_instead() {
    // A proxy reporting zeros would otherwise make a whole session look free,
    // and take the compaction-facing numbers down with it.
    let (_d, a, ctx) = harness(vec![turn_reporting("done", Usage::default())]);
    let totals = drive(&a, &ctx, "hi").await.1.unwrap().totals;

    assert!(
        totals.usage.input > 0,
        "input was left at the reported zero"
    );
    assert!(totals.usage.output > 0);
    assert!(totals.cost > 0.0);
    assert_eq!(
        totals.estimated,
        agent::Estimated {
            input: true,
            output: true,
            cache_read: false
        },
        "our own count must not pass as measured"
    );
}

#[tokio::test]
async fn only_the_half_the_provider_withheld_is_filled_in() {
    // Reporting one and not the other is the ordinary case, not a broken one:
    // the measured half has to survive intact.
    let reported = Usage {
        input: 4_321,
        output: 0,
        ..Default::default()
    };
    let (_d, a, ctx) = harness(vec![turn_reporting("done", reported)]);
    let totals = drive(&a, &ctx, "hi").await.1.unwrap().totals;

    assert_eq!(
        totals.usage.input, 4_321,
        "a measured count was overwritten"
    );
    assert!(totals.usage.output > 0);
    // Only the half that was withheld is marked; the stated one keeps its
    // standing.
    assert!(!totals.estimated.input);
    assert!(totals.estimated.output);
}

#[tokio::test]
async fn a_count_far_under_the_prompt_is_read_as_a_cache_miss() {
    // What a caching proxy does: a plausible-looking figure two orders of
    // magnitude under what was sent, with no cache field to explain it. The
    // figure is a measurement of the miss, so it survives, and the gap it
    // leaves is the hit the host declined to name.
    let reported = Usage {
        input: 12,
        output: 40,
        ..Default::default()
    };
    let (_d, a, ctx) = harness(vec![turn_reporting("done", reported)]);
    let totals = drive(&a, &ctx, "hi").await.1.unwrap().totals;

    assert_eq!(totals.usage.input, 12, "a measured count was overwritten");
    assert!(!totals.estimated.input);
    assert!(
        totals.usage.cache_read > 1_000,
        "{}",
        totals.usage.cache_read
    );
    assert!(
        totals.estimated.cache_read,
        "a count we made must travel marked"
    );
    // The part the host got right keeps its standing.
    assert_eq!(totals.usage.output, 40);
    assert!(!totals.estimated.output);
}

#[tokio::test]
async fn a_host_that_reports_nothing_is_counted_for() {
    // Zero is not a small number here: it is the absence of one. Nothing was
    // measured, so there is no miss to believe and no gap to attribute — the
    // whole prompt is ours to count, and travels marked as such.
    let (_d, a, ctx) = harness(vec![turn_reporting("done", Usage::default())]);
    let totals = drive(&a, &ctx, "hi").await.1.unwrap().totals;

    assert!(totals.usage.input > 1_000, "{}", totals.usage.input);
    assert!(totals.estimated.input, "a count we made must travel marked");
    assert_eq!(totals.usage.cache_read, 0, "nothing was said about a cache");
    assert!(!totals.estimated.cache_read);
}

#[tokio::test]
async fn a_small_count_beside_a_cached_prompt_is_left_alone() {
    // Cached input is excluded from the count by design, so twelve fresh
    // tokens beside a large cache figure is exactly right. Both halves of the
    // cache count for this: the first turn of a cached session writes the whole
    // prompt and reads none of it back, which is the case most easily mistaken
    // for a host reporting nonsense.
    for (read, write) in [(30_000, 0), (0, 30_000), (15_000, 15_000)] {
        let reported = Usage {
            input: 12,
            output: 40,
            cache_read: read,
            cache_write: write,
        };
        let (_d, a, ctx) = harness(vec![turn_reporting("done", reported)]);
        let totals = drive(&a, &ctx, "hi").await.1.unwrap().totals;

        assert_eq!(totals.usage.input, 12, "read={read} write={write}");
        assert!(!totals.estimated.input, "read={read} write={write}");
    }
}

#[tokio::test]
async fn the_model_is_told_how_many_turns_are_left() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "steady\n").unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    // Thirteen turns of work against a budget of thirteen: the notice lands at
    // ten left and again at three, and the run still ends on its own.
    let mut script: Vec<_> = (0..12).map(|_| call_turn(&[("t", "todo", "{}")])).collect();
    script.push(text_turn("done"));
    let mut a = Agent::new(Scripted::new(script), spec());
    a.max_turns = 13;

    let (session, out, _) = drive(&a, &ctx, "work").await;
    out.unwrap();

    let said: Vec<String> = session
        .context()
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect();

    assert!(
        said.iter().any(|t| t.contains("10 of 13 turns left")),
        "{said:?}"
    );
    assert!(said.iter().any(|t| t.contains("3 turns left")), "{said:?}");
}

#[tokio::test]
async fn a_fully_measured_turn_is_not_marked() {
    let (_d, a, ctx) = harness(vec![text_turn("done")]);
    let totals = drive(&a, &ctx, "hi").await.1.unwrap().totals;
    assert_eq!((totals.usage.input, totals.usage.output), (3_000, 5));
    assert!(!totals.estimated.any());
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
    assert_eq!(session.context().len(), 4);
    let view = session.context();
    let results = tool_results(&view[2]);
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
    let view = session.context();
    let results = tool_results(&view[2]);
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
    a.approver = Arc::new(Ceiling(Tier::Read));
    let (session, out, events) = drive(&a, &ctx, "run it").await;
    out.unwrap();

    let view = session.context();
    let results = tool_results(&view[2]);
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

    let view = session.context();
    let results = tool_results(&view[2]);
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
        session.context().len(),
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
    let Message::Assistant { content, .. } = &session.context()[1] else {
        panic!()
    };
    assert!(matches!(content[0], AssistantContent::Reasoning(_)));
}

/// Answers tool-bearing turns from a script and any tool-free turn — which is
/// what a summarization request is — with a fixed summary.
struct WithSummarizer {
    turns: Vec<Vec<StreamEvent>>,
    next: AtomicUsize,
    summaries: AtomicUsize,
}

#[async_trait]
impl Transport for WithSummarizer {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn stream(
        &self,
        _spec: &ModelSpec,
        req: &Request,
    ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
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

/// Weight in assistant prose, which elision cannot reclaim — only dropping the
/// exchange does, and that is what the summarizer exists for.
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
                provider: Some(ProviderCallId(id.into())),
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

    let mut a = Agent::new(transport.clone(), spec);
    a.max_turns = 6;

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
    assert!(session.log.messages().count() > view.len());
}

#[tokio::test]
async fn a_summarizer_that_fails_drops_the_history_without_failing_the_turn() {
    struct Broken(AtomicUsize);

    #[async_trait]
    impl Transport for Broken {
        fn name(&self) -> &'static str {
            "anthropic"
        }
        async fn stream(
            &self,
            _: &ModelSpec,
            req: &Request,
        ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
            if req.tools.is_empty() {
                return Err(brain::BrainError::Stream("summarizer is down".into()));
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
    let mut a = Agent::new(Arc::new(Broken(AtomicUsize::new(0))), spec);
    a.max_turns = 6;

    let (_session, out, events) = drive(&a, &ctx, "read it repeatedly").await;
    // Losing the summary costs context; failing the turn costs the whole run.
    out.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Compacted(r) if r.dropped > 0 && !r.summarized))
    );
}

/// Fails the first `fail` attempts with `err`, then answers normally.
struct Flaky {
    remaining: AtomicUsize,
    err: fn() -> brain::BrainError,
}

#[async_trait]
impl Transport for Flaky {
    fn name(&self) -> &'static str {
        "anthropic"
    }
    async fn stream(
        &self,
        _: &ModelSpec,
        _: &Request,
    ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) > 0 {
            return Err((self.err)());
        }
        Ok(futures::stream::iter(text_turn("recovered").into_iter().map(Ok)).boxed())
    }
}

fn flaky(times: usize, err: fn() -> brain::BrainError) -> Arc<Flaky> {
    Arc::new(Flaky {
        remaining: AtomicUsize::new(times),
        err,
    })
}

fn fast_retry(a: &mut Agent) {
    a.retry.base = std::time::Duration::from_millis(1);
    a.retry.max = std::time::Duration::from_millis(4);
}

#[tokio::test]
async fn a_throttled_request_is_retried_until_it_lands() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut a = Agent::new(
        flaky(2, || brain::BrainError::Api {
            transport: "anthropic",
            status: 429,
            body: "rate limit exceeded".into(),
        }),
        spec(),
    );
    fast_retry(&mut a);

    let (session, out, events) = drive(&a, &ctx, "go").await;
    out.unwrap();
    assert_eq!(session.context()[1].text(), "recovered");

    let retries: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::Retrying { .. }))
        .collect();
    assert_eq!(retries.len(), 2, "{retries:?}");
}

#[tokio::test]
async fn a_spent_quota_is_not_retried_however_much_it_looks_like_a_throttle() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut a = Agent::new(
        flaky(99, || brain::BrainError::Api {
            transport: "anthropic",
            status: 429,
            body: r#"{"error":{"code":"insufficient_quota"}}"#.into(),
        }),
        spec(),
    );
    fast_retry(&mut a);

    let (_s, out, events) = drive(&a, &ctx, "go").await;
    // The status is a throttle's; retrying it just spends money.
    assert!(out.is_err(), "{out:?}");
    assert!(
        !events.iter().any(|e| matches!(e, Event::Retrying { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn retries_give_up_rather_than_hammering_forever() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut a = Agent::new(
        flaky(99, || brain::BrainError::Stream("connection reset".into())),
        spec(),
    );
    fast_retry(&mut a);
    a.retry.attempts = 3;

    let (_s, out, events) = drive(&a, &ctx, "go").await;
    assert!(out.is_err());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Retrying { .. }))
            .count(),
        3
    );
}

/// Opens a stream and then never sends anything.
struct Wedged;

#[async_trait]
impl Transport for Wedged {
    fn name(&self) -> &'static str {
        "anthropic"
    }
    async fn stream(
        &self,
        _: &ModelSpec,
        _: &Request,
    ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
        Ok(futures::stream::pending().boxed())
    }
}

#[tokio::test]
async fn a_stream_that_stops_sending_does_not_hold_the_turn_open() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut a = Agent::new(Arc::new(Wedged), spec());
    fast_retry(&mut a);
    a.retry.attempts = 1;
    a.retry.idle = std::time::Duration::from_millis(120);

    let started = std::time::Instant::now();
    let (_s, out, events) = drive(&a, &ctx, "go").await;

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
        id: None,
        content: vec![brain::message::AssistantContent::ToolCall(
            brain::message::ToolCall {
                id: brain::message::ToolCallId(id.into()),
                provider: Some(ProviderCallId(id.into())),
                name: "read".into(),
                args: json!({ "path": "a.rs" }),
            },
        )],
    }
}

/// Refuses oversized requests until the transcript shrinks below `fits`.
struct Picky {
    fits: usize,
    refusals: AtomicUsize,
}

#[async_trait]
impl Transport for Picky {
    fn name(&self) -> &'static str {
        "anthropic"
    }
    async fn stream(
        &self,
        _: &ModelSpec,
        req: &Request,
    ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
        let size = brain::estimate::tokens(&req.messages);
        if size > self.fits {
            self.refusals.fetch_add(1, Ordering::SeqCst);
            return Err(brain::BrainError::Api {
                transport: "anthropic",
                status: 400,
                body: format!("prompt is too long: {size} tokens > {} maximum", self.fits),
            });
        }
        Ok(futures::stream::iter(text_turn("fits now").into_iter().map(Ok)).boxed())
    }
}

#[tokio::test]
async fn an_overflow_refusal_shrinks_the_transcript_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let picky = Arc::new(Picky {
        fits: 2_000,
        refusals: AtomicUsize::new(0),
    });

    let mut a = Agent::new(picky.clone(), spec());
    fast_retry(&mut a);
    // A transcript our own estimate calls comfortable, which the provider does
    // not — and one compaction can actually shrink, unlike a lone huge prompt.
    let mut history = vec![Message::user("the task")];
    for i in 0..3 {
        history.push(call_message(&format!("h{i}")));
        history.push(Message::tool_results(vec![
            brain::message::ToolResult::text(
                brain::message::ToolCallId(format!("h{i}")),
                "read",
                "z".repeat(12_000),
            ),
        ]));
    }
    let mut session = Session {
        log: agent::log::Log::from_messages(history),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out = a.run(&mut session, &ctx, &tx).await;
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
    // The refusal named its window, so the budget is refitted to it rather
    // than squeezed blindly.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Warning(w) if w.contains("2000-token window"))),
        "{events:?}"
    );
    // Compaction must land the transcript inside the window the refusal
    // named, comfortably — the head-and-tail floor lowers to a notice when
    // the budget cannot hold even a pruned result.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Compacted(r) if r.after < 1_000)),
        "{events:?}"
    );
    assert_eq!(session.context().last().unwrap().text(), "fits now");
}

/// Refuses without saying how big the window is.
struct Mute {
    fits: usize,
}

#[async_trait]
impl Transport for Mute {
    fn name(&self) -> &'static str {
        "anthropic"
    }
    async fn stream(
        &self,
        _: &ModelSpec,
        req: &Request,
    ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
        if brain::estimate::tokens(&req.messages) > self.fits {
            return Err(brain::BrainError::Api {
                transport: "anthropic",
                status: 413,
                body: "Request exceeds the maximum size".into(),
            });
        }
        Ok(futures::stream::iter(text_turn("ok").into_iter().map(Ok)).boxed())
    }
}

#[tokio::test]
async fn an_overflow_with_no_number_falls_back_to_squeezing() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    let mut history = vec![Message::user("the task")];
    for i in 0..3 {
        history.push(call_message(&format!("h{i}")));
        history.push(Message::tool_results(vec![
            brain::message::ToolResult::text(
                brain::message::ToolCallId(format!("h{i}")),
                "read",
                "z".repeat(12_000),
            ),
        ]));
    }

    // Squeezing blindly only corrects a modest error — three passes at 60% —
    // which is the realistic case: an estimate off by a third, not by 30x.
    let mut spec = spec();
    spec.context_window = 60_000;
    let mut a = Agent::new(Arc::new(Mute { fits: 8_000 }), spec);
    fast_retry(&mut a);
    let mut session = Session {
        log: agent::log::Log::from_messages(history),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out = a.run(&mut session, &ctx, &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }

    out.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Warning(w) if w.contains("named no limit"))),
        "{events:?}"
    );
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": { "count": { "type": "integer" }, "note": { "type": "string" } },
        "required": ["count"],
    })
}

fn yielding_agent(turns: Vec<Vec<StreamEvent>>) -> (tempfile::TempDir, Agent, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
    let mut a = Agent::new(Scripted::new(turns), spec());
    a.registry = Registry::builtin().with(tools::finish::Yield::new(schema()));
    a.finish_tool = Some(tools::finish::NAME.to_string());
    (dir, a, ctx)
}

#[tokio::test]
async fn a_yield_ends_the_run_and_hands_back_the_value() {
    let (_d, a, ctx) = yielding_agent(vec![
        call_turn(&[("y1", "yield", r#"{"count":3,"note":"three files"}"#)]),
        text_turn("never reached"),
    ]);
    let (_s, out, _) = drive(&a, &ctx, "count things").await;

    let value = out
        .unwrap()
        .yielded
        .expect("the run must hand back a value");
    assert_eq!(value["count"], 3);
    assert_eq!(value["note"], "three files");
}

#[tokio::test]
async fn nothing_runs_after_a_yield() {
    let (_d, a, ctx) = yielding_agent(vec![
        call_turn(&[("y1", "yield", r#"{"count":1}"#)]),
        call_turn(&[(
            "w1",
            "write",
            r#"{"path":"should-not-exist","content":"x"}"#,
        )]),
    ]);
    let (_s, out, _) = drive(&a, &ctx, "go").await;
    out.unwrap();
    assert!(!ctx.workspace.root().join("should-not-exist").exists());
}

#[tokio::test]
async fn a_missing_required_field_comes_back_as_a_result_to_fix() {
    let (_d, a, ctx) = yielding_agent(vec![
        call_turn(&[("y1", "yield", r#"{"note":"forgot the count"}"#)]),
        call_turn(&[("y2", "yield", r#"{"count":7}"#)]),
        text_turn("done"),
    ]);
    let (session, out, _) = drive(&a, &ctx, "go").await;

    assert_eq!(out.unwrap().yielded.unwrap()["count"], 7);
    let view = session.context();
    let first = tool_results(&view[2]);
    assert!(first[0].is_error);
    assert!(
        first[0]
            .flatten_text()
            .contains("missing required field(s): count"),
        "{first:?}"
    );
}

#[tokio::test]
async fn a_run_that_forgets_to_yield_is_reminded_once() {
    let (_d, a, ctx) = yielding_agent(vec![
        text_turn("here is my prose answer"),
        call_turn(&[("y1", "yield", r#"{"count":2}"#)]),
        text_turn("done"),
    ]);
    let (session, out, _) = drive(&a, &ctx, "go").await;

    assert_eq!(out.unwrap().yielded.unwrap()["count"], 2);
    assert!(
        session
            .context()
            .iter()
            .any(|m| m.text().contains("Call `yield` with it now")),
        "the reminder must be in the transcript"
    );
}

#[tokio::test]
async fn a_run_that_never_yields_fails_rather_than_returning_prose() {
    let (_d, a, ctx) = yielding_agent(vec![text_turn("prose"), text_turn("more prose")]);
    let (_s, out, _) = drive(&a, &ctx, "go").await;
    // One reminder, then the run is a failure: the caller asked for a value.
    assert!(matches!(out, Err(AgentError::NoResult)), "{out:?}");
}

#[test]
fn a_schema_that_is_not_an_object_is_refused_with_the_fix() {
    let err = tools::finish::check(&json!({ "type": "array" })).unwrap_err();
    assert!(err.contains("a tool input must be an object"), "{err}");
    assert!(
        err.contains(r#""result""#),
        "the message shows the wrapper: {err}"
    );
    assert!(tools::finish::check(&schema()).is_ok());
}

#[tokio::test]
async fn a_call_that_keeps_returning_the_same_thing_is_named() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "steady\n").unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    let same = || call_turn(&[("t", "read", r#"{"path":"a.txt"}"#)]);
    let a = Agent::new(
        Scripted::new(vec![same(), same(), same(), same(), text_turn("gave up")]),
        spec(),
    );

    let (session, out, _) = drive(&a, &ctx, "read it forever").await;
    out.unwrap();

    let bodies: Vec<String> = session
        .context()
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::ToolResult(r) => Some(r.flatten_text()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect();

    // The first repeat can be a legitimate re-read after compaction; by the
    // third the model is just stuck.
    assert!(!bodies[0].contains("same `read` call"), "{:?}", bodies[0]);
    assert!(!bodies[1].contains("same `read` call"), "{:?}", bodies[1]);
    assert!(
        bodies[2].contains("same `read` call with the same result, 3 times"),
        "{:?}",
        bodies[2]
    );
}

/// The shape a session actually dies in: a tool the model cannot get the
/// arguments right for, refused identically until the turn limit.
#[tokio::test]
async fn a_call_that_keeps_failing_the_same_way_is_named_sooner() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "steady\n").unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    let same = || call_turn(&[("t", "edit", r#"{"patch":"match code {"}"#)]);
    let a = Agent::new(
        Scripted::new(vec![same(), same(), same(), text_turn("gave up")]),
        spec(),
    );

    let (session, out, _) = drive(&a, &ctx, "edit it forever").await;
    out.unwrap();

    let bodies: Vec<String> = session
        .context()
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::ToolResult(r) => Some(r.flatten_text()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect();

    // No leeway for a refusal the way there is for a re-read: the second
    // identical failure is already the whole story.
    assert!(!bodies[0].contains("same `edit` call"), "{:?}", bodies[0]);
    assert!(
        bodies[1].contains("same `edit` call has now failed the same way 2 times"),
        "{:?}",
        bodies[1]
    );
    // And the notice rides inside the error the model reads, not beside it.
    assert!(bodies[1].starts_with("patch line 1"), "{:?}", bodies[1]);
}

#[tokio::test]
async fn a_call_whose_answer_changes_resets_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "first\n").unwrap();
    let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());

    let same = || call_turn(&[("t", "read", r#"{"path":"a.txt"}"#)]);
    let a = Agent::new(
        Scripted::new(vec![same(), same(), same(), text_turn("ok")]),
        spec(),
    );

    // Change the file between turns so the answer moves.
    let writer = {
        let path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = std::fs::write(&path, "second\n");
        })
    };
    let (session, out, _) = drive(&a, &ctx, "watch it").await;
    let _ = writer.await;
    out.unwrap();

    let noticed = session
        .context()
        .iter()
        .any(|m| m.text().contains("same `read` call"));
    // Whether the race lands or not, a changed answer must never be flagged.
    assert!(!noticed || std::fs::read_to_string(&path).unwrap() == "first\n");
}
