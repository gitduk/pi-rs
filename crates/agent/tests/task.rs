use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use brain::model::ModelSpec;
use brain::request::Request;
use brain::stream::{BlockKind, StopReason, StreamEvent, Usage};
use brain::transport::Transport;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};

mod common;
use common::spec;

use agent::session::Session;
use agent::task::{Home, Task};
use agent::{Agent, Totals};
use tools::{Ctx, FileLocks, FileShifts, Registry, Tier, Tool, ToolError, ToolOutput, Workspace};

struct Scripted {
    turns: Vec<Vec<StreamEvent>>,
    next: AtomicUsize,
    /// Every system prompt that went out, parent's and child's alike.
    saw: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Transport for Scripted {
    async fn stream(
        &self,
        _spec: &ModelSpec,
        req: &Request,
    ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
        if let Some(system) = &req.system {
            self.saw.lock().unwrap().push(system.clone());
        }
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        let events = self.turns.get(i).cloned().unwrap_or_default();
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

/// What a checkout says to whoever works in it, as `resolve` composes it.
const STANDING: &str = "\n\n<workspace path=\"/anywhere\"/>\nnever touch infra/\n";

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
                input: 1_000,
                output: 7,
                ..Default::default()
            },
        },
    ]
}

fn call_turn(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall {
                id: Some(id.into()),
                name: name.into(),
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 0,
            delta: args.into(),
        },
        StreamEvent::Done {
            stop: StopReason::ToolUse,
            usage: Usage {
                input: 500,
                output: 3,
                ..Default::default()
            },
        },
    ]
}

/// What the child's `Ctx` turned out to be, captured from inside its own run —
/// the only place the split in §4.1 is observable.
#[derive(Default)]
struct Seen {
    locks: std::sync::Mutex<Option<FileLocks>>,
    shifts: std::sync::Mutex<Option<FileShifts>>,
    root: std::sync::Mutex<Option<std::path::PathBuf>>,
    namespace: std::sync::Mutex<Option<String>>,
}

struct Probe {
    seen: Arc<Seen>,
    /// Tripped from inside the child's run, which is where Esc arrives from
    /// the child's point of view.
    trip: Option<tokio_util::sync::CancellationToken>,
}

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "records the ctx it was handed"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    async fn execute(&self, _args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        *self.seen.locks.lock().unwrap() = Some(ctx.file_locks.clone());
        *self.seen.shifts.lock().unwrap() = Some(ctx.file_shifts.clone());
        *self.seen.root.lock().unwrap() = Some(ctx.workspace.root().to_path_buf());
        *self.seen.namespace.lock().unwrap() = Some(ctx.spill_namespace().to_string());
        if let Some(trip) = &self.trip {
            trip.cancel();
        }
        Ok(ToolOutput::text("probed"))
    }
}

/// Finishes only when its token is tripped, so a test of the limits does not
/// have to win a race with a scripted stream that is ready the instant it is
/// polled. A real provider is never that fast, which is why the limits work in
/// production and cannot be observed here any other way.
struct Sleeper;

#[async_trait]
impl Tool for Sleeper {
    fn name(&self) -> &str {
        "sleeper"
    }
    fn description(&self) -> &str {
        "waits to be stopped"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    async fn execute(&self, _args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        ctx.cancel.cancelled().await;
        Err(ToolError::Cancelled)
    }
}

#[derive(Default)]
struct Kept {
    /// The child's whole transcript, the only way anything outside it can see
    /// what it was allowed to do.
    sessions: std::sync::Mutex<Vec<(String, String)>>,
}

impl Home for Kept {
    fn keep(&self, id: &str, session: Session) {
        self.sessions
            .lock()
            .unwrap()
            .push((id.to_string(), format!("{:?}", session.entries())));
    }
}

/// Parent and child share one scripted transport, so the turns run in the order
/// written: the parent's call, then the whole child, then the parent's answer.
fn harness(turns: Vec<Vec<StreamEvent>>) -> (tempfile::TempDir, Agent, Ctx, Arc<Seen>, Arc<Kept>) {
    rigged(turns, 20, std::time::Duration::from_secs(600), false)
}

fn rigged(
    turns: Vec<Vec<StreamEvent>>,
    max_turns: usize,
    deadline: std::time::Duration,
    esc: bool,
) -> (tempfile::TempDir, Agent, Ctx, Arc<Seen>, Arc<Kept>) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let ctx = Ctx::new(ws);
    let seen = Arc::new(Seen::default());
    let kept = Arc::new(Kept::default());
    let mut parent = Agent::new(
        Arc::new(Scripted {
            turns,
            next: AtomicUsize::new(0),
            saw: Arc::default(),
        }),
        spec(),
    );
    parent.registry = Registry::new()
        .with(Sleeper)
        .with(tools::write::Write)
        .with(Probe {
            seen: seen.clone(),
            trip: esc.then(|| ctx.cancel.clone()),
        });
    let task = Task::new(&parent, kept.clone(), STANDING).with_limits(max_turns, deadline);
    parent.registry = parent.registry.clone().with(task);
    (dir, parent, ctx, seen, kept)
}

async fn drive(
    agent: &Agent,
    ctx: &Ctx,
    prompt: &str,
) -> (Session, Result<Totals, agent::AgentError>) {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut session = Session::with_prompt(prompt);
    let out = agent.run(&mut session, ctx, &tx).await;
    (session, out)
}

#[tokio::test]
async fn the_child_answers_into_the_parents_transcript() {
    let (_dir, agent, ctx, seen, kept) = harness(vec![
        call_turn("c1", "task", r#"{"description":"count the files","prompt":"count the files"}"#),
        call_turn("c2", "probe", "{}"),
        text_turn("there are four files"),
        text_turn("the subagent says four"),
    ]);
    let (session, out) = drive(&agent, &ctx, "how many files").await;

    // D5: the child's spend rides home on the tool result, so the parent's run
    // pays for it — the parent's and child's two turns are 3000 in / 20 out.
    let totals = out.expect("the parent's run ends");
    assert_eq!(
        (totals.usage.input, totals.usage.output),
        (3_000, 20),
        "the run that called the child counts its spend: {totals:?}"
    );
    assert!(totals.cost > 0.0, "{totals:?}");

    let transcript = format!("{:?}", session.entries());
    assert!(
        transcript.contains("there are four files"),
        "the child's last words are the tool result: {transcript}"
    );
    // D7: one transcript filed, under a namespace of the child's own.
    let sessions = kept.sessions.lock().unwrap();
    assert_eq!(sessions.len(), 1, "one child, one transcript");
    assert!(
        sessions[0].1.contains("count the files"),
        "the filed transcript is the child's own: {}",
        sessions[0].1
    );
    let ns = seen.namespace.lock().unwrap().clone().unwrap();
    assert!(ns.contains("task"), "the child files under its own name: {ns}");
    assert_ne!(ns, ctx.spill_namespace(), "and not the parent's");
}

/// The row a finished call leaves has to say which job ended. Several
/// children run at once, and their bills are indistinguishable.
#[tokio::test]
async fn a_finished_call_still_names_the_job() {
    let (_dir, parent, ctx, _seen, kept) = harness(vec![text_turn("four")]);
    let task = Task::new(&parent, kept, STANDING);
    let out = task
        .execute(
            json!({ "description": "count the files", "prompt": "count them" }),
            &ctx,
        )
        .await
        .expect("the child ran");

    let sketch = out.preview();
    assert_eq!(
        sketch, "count the files [1 turn · 1.0k/7]",
        "the job leads and the bill is ranked under it"
    );

    // A description the model left blank leaves the cost alone rather than a
    // separator with nothing in front of it.
    let (_dir, parent, ctx, _seen, kept) = harness(vec![text_turn("four")]);
    let task = Task::new(&parent, kept, STANDING);
    let bare = task
        .execute(json!({ "description": " ", "prompt": "count them" }), &ctx)
        .await
        .expect("the child ran")
        .preview();
    assert_eq!(bare, "1 turn · 1.0k/7", "no brackets around a bare bill: {bare}");
}

#[tokio::test]
async fn the_child_shares_the_tree_and_its_bookkeeping() {
    let (_dir, agent, ctx, seen, _kept) = harness(vec![
        call_turn("c1", "task", r#"{"description":"look","prompt":"look"}"#),
        call_turn("c2", "probe", "{}"),
        text_turn("looked"),
        text_turn("done"),
    ]);
    let _ = drive(&agent, &ctx, "go").await;

    // §4.1, the three that must be one: same tree, and the two tables that
    // record what has been written and renumbered in it. Sharing the tree while
    // splitting the tables is how two writers both succeed and one edit
    // vanishes, which is exactly what lanes never have to worry about.
    assert_eq!(
        seen.root.lock().unwrap().clone().unwrap(),
        ctx.workspace.root(),
    );
    assert!(Arc::ptr_eq(
        seen.locks.lock().unwrap().as_ref().unwrap(),
        &ctx.file_locks
    ));
    assert!(Arc::ptr_eq(
        seen.shifts.lock().unwrap().as_ref().unwrap(),
        &ctx.file_shifts
    ));
}

#[tokio::test]
async fn the_child_cannot_send_out_a_child_of_its_own() {
    let (_dir, agent, ctx, _seen, kept) = harness(vec![
        call_turn("c1", "task", r#"{"description":"delegate this","prompt":"delegate this"}"#),
        // The child tries to do the same thing to someone else.
        call_turn("c2", "task", r#"{"description":"no you","prompt":"no you"}"#),
        text_turn("could not"),
        text_turn("fine"),
    ]);
    let _ = drive(&agent, &ctx, "go").await;

    // D3 checked where it bites rather than by reading a list: the child asked,
    // and the registry it was given had no such tool to give it.
    let sessions = kept.sessions.lock().unwrap();
    assert_eq!(sessions.len(), 1, "one child, and it spawned none");
    assert!(
        sessions[0].1.contains("no tool named"),
        "the child was refused its own `task`: {}",
        sessions[0].1
    );
}

#[tokio::test]
async fn a_child_that_says_nothing_still_says_something() {
    let (_dir, agent, ctx, _seen, _kept) = harness(vec![
        call_turn("c1", "task", r#"{"description":"be quiet","prompt":"be quiet"}"#),
        vec![StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }],
        text_turn("nothing came back"),
    ]);
    let (session, _out) = drive(&agent, &ctx, "go").await;
    let transcript = format!("{:?}", session.entries());
    // An empty tool result is a thing the caller cannot read; a sentence
    // saying it ran and produced nothing is.
    assert!(
        transcript.contains("without saying anything"),
        "{transcript}"
    );
}

#[tokio::test]
async fn running_out_of_time_is_an_answer_not_a_failure() {
    let (_dir, agent, ctx, _seen, _kept) = rigged(
        vec![
            call_turn("c1", "task", r#"{"description":"go round","prompt":"go round"}"#),
            call_turn("c2", "sleeper", "{}"),
            text_turn("it reported back"),
        ],
        20,
        std::time::Duration::from_millis(50),
        false,
    );
    let (session, out) = drive(&agent, &ctx, "go").await;

    // D10's whole point: the work done before the limit is still work. Handing
    // it back as an error means the caller paid for it and got nothing — and
    // handing it back as `Cancelled` would end the caller's turn outright.
    assert!(out.is_ok(), "the caller's turn survives a child that ran out");
    let transcript = format!("{:?}", session.entries());
    assert!(transcript.contains("unfinished"), "{transcript}");
    assert!(transcript.contains("stopped after"), "{transcript}");
}

#[tokio::test]
async fn esc_reaches_through_the_child_and_ends_the_callers_turn() {
    let (_dir, agent, ctx, _seen, _kept) = rigged(
        vec![
            call_turn("c1", "task", r#"{"description":"long one","prompt":"long one"}"#),
            call_turn("c2", "probe", "{}"),
            call_turn("c3", "sleeper", "{}"),
            text_turn("never gets here"),
        ],
        20,
        std::time::Duration::from_secs(600),
        true,
    );
    let (_session, out) = drive(&agent, &ctx, "go").await;

    // The one mapping that must not be got wrong. `Cancelled` is the single
    // error the loop never hands back to the model, so a cap answering with it
    // would silently end the caller's turn — and Esc answering with anything
    // else would leave the caller talking to a model the user just stopped.
    assert!(
        matches!(out, Err(agent::AgentError::Cancelled)),
        "esc ends the whole turn, not just the child: {out:?}"
    );
}

#[tokio::test]
async fn the_child_gets_no_tool_the_parent_was_denied() {
    // The parent is built with `probe` only, as `--tools` would leave it. The
    // child is cloned from that, so a restriction the user asked for cannot be
    // stepped around by delegating — which would otherwise make `task` a way
    // to get back everything `--tools` took away.
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let seen = Arc::new(Seen::default());
    let kept = Arc::new(Kept::default());
    let mut parent = Agent::new(
        Arc::new(Scripted {
            turns: vec![
                call_turn("c1", "task", r#"{"description":"try it","prompt":"try it"}"#),
                call_turn("c2", "sleeper", "{}"),
                text_turn("could not"),
                text_turn("nor could I"),
            ],
            next: AtomicUsize::new(0),
            saw: Arc::default(),
        }),
        spec(),
    );
    parent.registry = Registry::new().with(Probe {
        seen: seen.clone(),
        trip: None,
    });
    parent.registry = parent
        .registry
        .clone()
        .with(Task::new(&parent, kept.clone(), STANDING));

    let _ = drive(&parent, &Ctx::new(ws), "go").await;

    let sessions = kept.sessions.lock().unwrap();
    assert!(
        sessions[0].1.contains("no tool named"),
        "the child inherited the parent's restriction: {}",
        sessions[0].1
    );
}

#[tokio::test]
async fn the_child_is_told_what_the_checkout_says() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let kept = Arc::new(Kept::default());
    let saw: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let mut parent = Agent::new(
        Arc::new(Scripted {
            turns: vec![
                call_turn("c1", "task", r#"{"description":"go","prompt":"go"}"#),
                text_turn("done"),
                text_turn("the subagent is done"),
            ],
            next: AtomicUsize::new(0),
            saw: saw.clone(),
        }),
        spec(),
    );
    parent.system = format!("You are a coding agent.{STANDING}");
    parent.registry = parent
        .registry
        .clone()
        .with(Task::new(&parent, kept.clone(), STANDING));

    let _ = drive(&parent, &Ctx::new(ws), "go").await;

    let saw = saw.lock().unwrap();
    let child = saw
        .iter()
        .find(|s| s.contains("You are a subagent"))
        .expect("the child asked for a turn");
    assert!(
        child.contains("<workspace path=\"/anywhere\"/>") && child.contains("never touch infra/"),
        "the child stands on the same checkout as its caller: {child}"
    );
    assert!(
        !child.contains("You are a coding agent"),
        "but not on its caller's prompt: {child}"
    );
}

/// The child's account of its own work is the one part of the result nothing
/// else checks, and it cannot be asked again. What the tree recorded as the
/// child wrote it comes back beside that account, whatever the account says.
#[tokio::test]
async fn what_the_child_wrote_comes_back_beside_what_it_says() {
    let (_dir, agent, ctx, _seen, _kept) = harness(vec![
        // The parent writes first, in the tree the child is about to share.
        call_turn("p0", "write", r#"{"path":"parent.rs","content":"pub fn p() {}\n"}"#),
        call_turn("c1", "task", r#"{"description":"add one","prompt":"add one"}"#),
        call_turn("c2", "write", r#"{"path":"child.rs","content":"pub fn c() {}\n"}"#),
        text_turn("I rewrote the whole crate"),
        text_turn("it says it rewrote the crate"),
    ]);
    let (session, _out) = drive(&agent, &ctx, "go").await;
    let transcript = format!("{:?}", session.entries());

    // Read out of the result rather than searched for across the transcript,
    // which holds the caller's own write as well and would match it.
    let (_, after) = transcript.split_once("[wrote ").expect("a ledger: {transcript}");
    let ledger = after.split_once(']').expect("a closed ledger").0;
    // The two runs share a tree but not a record. A shared one would hand the
    // caller its own edit back as the child's — worse than no record at all,
    // because it reads as corroboration.
    assert_eq!(
        ledger, "1 file: child.rs",
        "the child's writes, and only those: {transcript}"
    );
}

#[tokio::test]
async fn a_child_that_wrote_nothing_is_said_to_have_written_nothing() {
    let (_dir, parent, ctx, _seen, kept) = harness(vec![text_turn("all done, fixed it")]);
    let task = Task::new(&parent, kept, STANDING);
    let out = task
        .execute(json!({ "description": "fix it", "prompt": "fix it" }), &ctx)
        .await
        .expect("the child ran")
        .flatten();

    // The line worth having: it has just described changes it did not make.
    assert!(out.contains("[wrote nothing]"), "{out}");
}

#[tokio::test]
async fn a_check_that_passes_does_not_drag_its_output_along() {
    let (_dir, parent, ctx, _seen, kept) = harness(vec![text_turn("did it")]);
    std::fs::write(ctx.workspace.root().join("out.txt"), "SPECIMEN\n").unwrap();
    let task = Task::new(&parent, kept, STANDING);
    let out = task
        .execute(
            json!({ "description": "go", "prompt": "go", "verify": "cat out.txt" }),
            &ctx,
        )
        .await
        .expect("the child ran")
        .flatten();

    assert!(out.contains("[verify `cat out.txt`: exit 0]"), "{out}");
    assert!(
        !out.contains("SPECIMEN"),
        "a check that passed says all it has to with its status: {out}"
    );
}

#[tokio::test]
async fn a_check_that_fails_comes_back_with_what_it_printed() {
    let (_dir, parent, ctx, _seen, kept) = harness(vec![text_turn("did it")]);
    std::fs::write(ctx.workspace.root().join("out.txt"), "SPECIMEN\n").unwrap();
    let task = Task::new(&parent, kept, STANDING);
    let out = task
        .execute(
            json!({ "description": "go", "prompt": "go", "verify": "cat out.txt; exit 3" }),
            &ctx,
        )
        .await
        .expect("the child ran")
        .flatten();

    assert!(out.contains("[verify `cat out.txt; exit 3`: exit 3]"), "{out}");
    assert!(out.contains("SPECIMEN"), "a failing check is what was asked for: {out}");
    assert!(out.contains("did it"), "and the child still gets its say: {out}");
}

#[tokio::test]
async fn a_child_that_ran_out_of_time_is_still_checked() {
    let (_dir, parent, ctx, _seen, kept) = rigged(
        vec![call_turn("c1", "sleeper", "{}"), text_turn("never")],
        20,
        std::time::Duration::from_millis(50),
        false,
    );
    let task = Task::new(&parent, kept, STANDING)
        .with_limits(20, std::time::Duration::from_millis(50));
    let out = task
        .execute(
            json!({ "description": "go", "prompt": "go", "verify": "true" }),
            &ctx,
        )
        .await
        .expect("the caller's turn survives")
        .flatten();

    assert!(out.contains("unfinished"), "{out}");
    // The child's own token is tripped by its deadline. Running the check
    // against that token answers `cancelled` at the one ending whose state
    // nobody can vouch for — which is the ending a check is most wanted at.
    assert!(out.contains("[verify `true`: exit 0]"), "{out}");
}
