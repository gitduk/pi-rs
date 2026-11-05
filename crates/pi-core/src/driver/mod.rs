//! What drives a lane from outside the keyboard: an execution, a person through
//! one, a program claiming the checkout's turns.
//!
//! A driver hears the progress and end of turns its own lines began; `Drivers`
//! keeps that ledger so the surface need not know which driver cares.

pub mod background;
pub mod input;

use std::sync::Arc;

use crate::execution::Request;
use agent::Event;
use tool::Ctx;
use toolbox::channel::Response;
use toolbox::channel::Told;

/// Who sent a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Typed,
    Background,
    /// A line an execution submitted, as if typed: the operation's answer
    /// goes back to it.
    Input(u64),
}

/// How a turn ended, as every driver that began it is told.
#[derive(Debug)]
pub enum Ended {
    Done,
    // The user stopped it, or a stop was asked for another way.
    Stopped,
    // The prompt was taken back before the model said a word.
    Unsent,
    Failed(String),
}

impl Ended {
    pub fn outcome(&self) -> toolbox::channel::Outcome {
        match self {
            Self::Done => toolbox::channel::Outcome::Done,
            Self::Stopped | Self::Unsent => toolbox::channel::Outcome::Stopped(String::new()),
            Self::Failed(why) => toolbox::channel::Outcome::Failed(why.clone()),
        }
    }
}

/// What a command control answers, and where it goes.
pub enum Said {
    // A refusal or what did not happen: a notice, not an answer.
    Notice(String),
    // A notice (empty when there is none) and a note the model reads before
    // it next speaks: something it started was changed under it.
    Noted(String, String),
}

/// A line a driver sends a free lane, and the note that goes before it.
pub struct Next {
    pub line: String,
    pub note: String,
    pub origin: Origin,
    /// Set when the line is the model's own come back, not the user's: what
    /// the screen names it by. The note then rides with the ask.
    pub label: Option<String>,
}

#[derive(Default)]
pub struct Drivers {
    executions: Arc<background::Table>,
    // Runs an execution is told the end of — its input opened them, or it follows
    // its checkout — each with its answer so far.
    owed: Vec<Owed>,
    running: std::collections::HashMap<u64, Request>,
}

struct Owed {
    execution: u64,
    lane: u64,
    said: String,
    // Where the current turn's text began: a retried request starts over.
    turn_from: usize,
    sent: usize,
    progress: Option<(String, tokio::time::Instant)>,
    request: u64,
}

impl Drivers {
    /// Take a submitted user line without consuming background results.
    pub fn next_input(&self, root: &std::path::Path) -> Option<Next> {
        let (execution, background::Back::Input { text, note }) =
            self.executions.take_input(root)?
        else {
            unreachable!("only user submissions were requested")
        };
        Some(Next {
            line: text,
            note,
            origin: Origin::Input(execution),
            label: None,
        })
    }

    /// Start an operation independently of the other requests on this lane.
    pub fn started(&mut self, request: Request, model: bool) {
        if model {
            self.running.insert(request.lane, request);
            match request.origin {
                Origin::Input(execution) => self.owe_request(execution, request.lane, request.id),
                Origin::Typed | Origin::Background => {}
            }
        } else {
            self.respond(request, Response::Started);
        }
    }

    /// Route a rendered response to its submitting execution. Local rendering
    /// is the caller's; this layer knows neither layouts nor terminal state.
    pub fn respond(&self, request: Request, event: Response) {
        if let Origin::Input(execution) = request.origin {
            self.executions.tell(
                execution,
                Told::Response {
                    request: request.id,
                    event,
                },
            );
        }
    }

    /// What the shell controls and calls that outlive their turn write into: the
    /// lanes' tools and this driver share it.
    pub fn background(&self) -> Arc<background::Table> {
        self.executions.clone()
    }

    /// The line a driver sends `lane`, the checkout at `root`, next. Asked
    /// only when the lane is free and nothing typed is waiting: what the user
    /// says comes first.
    ///
    /// Queued execution results first: they were due already, and an input of
    /// a driving execution can wait one turn. Inputs keep their own turns.
    pub fn next(&mut self, root: &std::path::Path, ctx: &Ctx) -> Option<Next> {
        Some(match self.executions.take_back(root)? {
            (_, background::Back::Result(due)) => Next {
                note: due.note_for(ctx),
                line: due.line,
                origin: Origin::Background,
                label: Some(due.label),
            },
            (execution, background::Back::Input { text, note }) => Next {
                line: text,
                note,
                origin: Origin::Input(execution),
                label: None,
            },
        })
    }

    /// One event from any lane's run, on the checkout at `root`; each driver
    /// keeps its own turns'.
    pub fn observe(&mut self, lane: u64, root: &std::path::Path, event: &Event) {
        // Each request, not just the run's first: one that follows from
        // mid-run still hears how the run ends.
        if let Event::TurnStart { .. } = event {
            let request = *self
                .running
                .entry(lane)
                .or_insert_with(|| Request::new(Origin::Background, lane));
            for execution in self.executions.followers(root) {
                self.owe_request(execution, lane, request.id);
            }
        }
        for owed in self.owed.iter_mut().filter(|o| o.lane == lane) {
            match event {
                Event::TextDelta(text) => owed.said.push_str(text),
                Event::TurnStart { .. } => owed.turn_from = owed.said.len(),
                Event::Retrying { .. } => owed.said.truncate(owed.turn_from),
                Event::ToolQueued { .. } => {
                    let text = &owed.said[owed.sent..];
                    if !text.trim().is_empty() {
                        self.executions.tell(
                            owed.execution,
                            owed.response(Response::Progress(text.to_string())),
                        );
                        owed.sent = owed.said.len();
                    }
                }
                Event::ToolProgress { text, .. } if !text.trim().is_empty() => {
                    let now = tokio::time::Instant::now();
                    if owed.progress.as_ref().is_none_or(|(last, at)| {
                        last != text && now.duration_since(*at) >= background::PROGRESS_EVERY
                    }) {
                        self.executions.tell(
                            owed.execution,
                            owed.response(Response::Progress(text.clone())),
                        );
                        owed.progress = Some((text.clone(), now));
                    }
                }
                _ => {}
            }
        }
    }

    /// A run on `lane` ended.
    pub fn turn_ended(&mut self, lane: u64, ended: &Ended) {
        self.running.remove(&lane);
        for owed in self.owed.extract_if(.., |o| o.lane == lane) {
            self.executions.tell(
                owed.execution,
                owed.response(Response::Finished {
                    text: owed.said[owed.sent..].into(),
                    outcome: ended.outcome(),
                }),
            );
        }
    }

    /// Whether a driver will send the lane at `root` more: it has not finished.
    pub fn holds(&self, root: &std::path::Path) -> bool {
        self.executions.driving(root)
    }

    /// End the runs of lanes that are gone.
    pub fn retain(&mut self, live: impl Fn(u64) -> bool) {
        self.running.retain(|lane, _| live(*lane));
        // A command left waiting on a reply would wait for good.
        for owed in self.owed.extract_if(.., |o| !live(o.lane)) {
            self.executions.tell(
                owed.execution,
                owed.response(Response::Finished {
                    text: owed.said[owed.sent..].into(),
                    outcome: Ended::Stopped.outcome(),
                }),
            );
        }
    }
}

impl Drivers {
    // A command is owed a run's answer once, however many reasons it has.
    fn owe_request(&mut self, execution: u64, lane: u64, request: u64) {
        if self
            .owed
            .iter()
            .any(|o| o.execution == execution && o.lane == lane)
        {
            return;
        }
        self.owed.push(Owed {
            execution,
            lane,
            said: String::new(),
            turn_from: 0,
            sent: 0,
            progress: None,
            request,
        });
        self.executions.tell(
            execution,
            Told::Response {
                request,
                event: Response::Started,
            },
        );
    }
}

impl Owed {
    fn response(&self, event: Response) -> Told {
        Told::Response {
            request: self.request,
            event,
        }
    }
}

// What the user's controls changed: failures are notices; successful
// changes are also told to the model, since the executions will not report them.
fn command_note(
    (failed, done): (Vec<String>, Vec<String>),
    note: impl Fn(&str, bool) -> String,
) -> Said {
    let failed = crate::input::notice(failed);
    if done.is_empty() {
        return Said::Notice(failed);
    }
    Said::Noted(failed, note(&done.join(", "), done.len() == 1))
}

impl Drivers {
    /// Stop the named executions, or all running executions when none are named, and
    /// tell the model what the user changed.
    pub fn stop_commands(&self, root: &std::path::Path, ids: &[u64]) -> Said {
        command_note(self.executions.stop_each(root, ids), |ids, one| {
            let (execution, it) = if one {
                ("command", "it")
            } else {
                ("commands", "them")
            };
            format!(
                "The user stopped background {execution} {ids}. No further results will arrive from {it}."
            )
        })
    }

    /// Restart the named executions and tell the model that their earlier,
    /// unreported results no longer apply.
    pub fn restart_commands(&self, root: &std::path::Path, ids: &[u64]) -> Said {
        command_note(self.executions.restart_each(root, ids), |ids, one| {
            let execution = if one { "command" } else { "commands" };
            format!(
                "The user restarted background {execution} {ids}: each runs again under the same id, and what the earlier run had not yet reported is dropped."
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tool::Tool as _;
    use toolbox::background::RunSink as _;
    use toolbox::channel::Outcome;

    // A background command belongs to the execution. Ending the turn that
    // started it does not cancel it; completion opens a later turn through the
    // same driver.
    #[tokio::test]
    async fn a_background_command_returns_before_it_ends_and_reports_in_a_later_turn() {
        use serde_json::json;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let root = ctx.workspace.root();
        let mut drivers = Drivers::default();
        let table = drivers.background();
        let control = Arc::new(background::Commands::new(table.clone()));
        let executions = toolbox::bash::Bash::new(control.clone()).with_controls(control);
        let marker = root.join("started");
        let out = tokio::time::timeout(
            Duration::from_millis(500),
            executions.execute(
                json!({"command": "sleep 0.4; echo started > started; echo finished", "background": true}),
                &ctx,
            ),
        )
        .await
        .expect("starting must not wait for the command")
        .unwrap();
        assert!(out.flatten().contains("command #1"));
        assert!(!marker.exists(), "the command has not finished");
        assert!(drivers.next(root, &ctx).is_none());
        ctx.cancel.cancel();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "ending the turn does not stop it");
        let next = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = table.changed().notified();
                if let Some(next) = drivers.next(root, &ctx) {
                    break next;
                }
                changed.await;
            }
        })
        .await
        .expect("completion is delivered without another user input");
        assert_eq!(next.origin, Origin::Background);
        assert!(next.line.contains("finished"));
        assert!(marker.exists());
        assert!(drivers.next(root, &ctx).is_none(), "reported once");
    }

    #[tokio::test]
    async fn queued_results_open_one_turn_only_on_their_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let mut drivers = Drivers::default();
        let root = std::path::Path::new("/checkout");
        let elsewhere = std::path::Path::new("/elsewhere");
        let table = drivers.background();
        let sink = background::Commands::new(table.clone());
        let first = sink.start(
            root.into(),
            toolbox::background::Invocation::shell("build"),
            Default::default(),
        );
        let other = sink.start(
            elsewhere.into(),
            toolbox::background::Invocation::shell("other"),
            Default::default(),
        );
        let second = sink.start(
            root.into(),
            toolbox::background::Invocation::shell("test"),
            Default::default(),
        );
        // Arrival order differs from registration order, as it can in a run.
        second.channel().result("tests passed".into());
        second.end();
        other.channel().result("another checkout".into());
        other.end();
        first.channel().result("built".into());
        first.end();

        let next = drivers.next(root, &ctx).unwrap();
        assert_eq!(next.origin, Origin::Background);
        assert_eq!(next.label.as_deref(), Some("2 background command results"));
        assert_eq!(
            next.line,
            "Background command #1 (build) reports:\n\nbuilt\n\nBackground command #3 (test) reports:\n\ntests passed"
        );
        assert!(next.note.contains("2 queued results"));
        assert!(
            drivers.next(root, &ctx).is_none(),
            "one turn drains the backlog"
        );
        assert!(
            table.background(root).is_empty(),
            "ended executions are settled"
        );
        let next = drivers.next(elsewhere, &ctx).unwrap();
        assert!(next.line.ends_with("another checkout"));
        assert!(next.note.contains("background command #2 (other)"));
        assert!(drivers.next(elsewhere, &ctx).is_none());
    }

    // What the run wrote after a command started is named beside its result;
    // what it wrote before is not.
    #[tokio::test]
    async fn a_result_names_the_files_written_after_its_command_started() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let root = ctx.workspace.root();
        let mut drivers = Drivers::default();
        let sink = background::Commands::new(drivers.background());
        ctx.note_write(&root.join("before.rs"));
        std::thread::sleep(std::time::Duration::from_millis(5));
        let build = sink.start(
            root.into(),
            toolbox::background::Invocation::shell("build"),
            Default::default(),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        ctx.note_write(&root.join("after.rs"));
        build.channel().result("built".into());
        build.end();

        let note = drivers.next(root, &ctx).unwrap().note;
        assert!(
            note.contains("You changed these files after build #1 started: `after.rs`."),
            "{note}"
        );
        assert!(!note.contains("before.rs"), "{note}");
    }

    #[tokio::test]
    async fn batching_preserves_inputs_and_leaves_future_results_for_later() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let mut drivers = Drivers::default();
        let root = std::path::Path::new("/checkout");
        let sink = background::Commands::new(drivers.background());
        let execution = sink.start(
            root.into(),
            toolbox::background::Invocation::shell("watch"),
            Default::default(),
        );
        let other = sink.start(
            root.into(),
            toolbox::background::Invocation::shell("build"),
            Default::default(),
        );
        execution.channel().result("first".into());
        execution.channel().result("second".into());
        execution
            .channel()
            .input("what happened?".into(), String::new());
        execution.channel().result("third".into());
        execution.channel().input("and now?".into(), String::new());
        other.channel().result("built".into());
        other.end();

        let batch = drivers.next(root, &ctx).unwrap();
        assert_eq!(batch.label.as_deref(), Some("2 background command results"));
        assert!(batch.line.ends_with("second"));
        assert!(!batch.line.contains("third"));
        let input = drivers.next(root, &ctx).unwrap();
        assert_eq!(input.origin, Origin::Input(execution.id()));
        assert_eq!(input.line, "what happened?");
        assert!(input.label.is_none());
        assert!(input.note.is_empty());
        assert!(drivers.next(root, &ctx).unwrap().line.ends_with("third"));
        let input = drivers.next(root, &ctx).unwrap();
        assert_eq!(input.origin, Origin::Input(execution.id()));
        assert_eq!(input.line, "and now?");
        assert!(drivers.next(root, &ctx).unwrap().line.ends_with("built"));
        assert!(drivers.next(root, &ctx).is_none());

        execution.channel().result("later".into());
        execution.end();
        let next = drivers.next(root, &ctx).unwrap();
        assert!(next.line.ends_with("later"));
        assert!(next.note.contains("background command #1 (watch)"));
        assert!(drivers.next(root, &ctx).is_none());
    }

    // A note rides with the line: the model reads it before the round.
    #[tokio::test]
    async fn an_input_carries_its_note_to_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let mut drivers = Drivers::default();
        let root = std::path::Path::new("/checkout");
        let sink = background::Commands::new(drivers.background());
        let execution = sink.start(
            root.into(),
            toolbox::background::Invocation::shell("driver"),
            Default::default(),
        );
        execution.channel().drive(true);
        execution
            .channel()
            .input("/code-review".into(), "This is loop round 2.".into());
        assert!(drivers.holds(root), "a driving execution holds its lane");
        let next = drivers.next(root, &ctx).unwrap();
        assert_eq!(next.origin, Origin::Input(execution.id()));
        assert_eq!(next.line, "/code-review");
        assert_eq!(next.note, "This is loop round 2.");
    }

    #[tokio::test]
    async fn progress_reaches_the_command_before_tools_without_repeating_in_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut drivers = Drivers::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let execution = background::Commands::new(drivers.background()).start(
            root.clone(),
            toolbox::background::Invocation::shell("bridge"),
            Default::default(),
        );
        execution.channel().listen(tx);
        let request = Request::new(Origin::Input(execution.id()), 1);
        drivers.started(request, true);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Started
            })
        );
        drivers.observe(1, &root, &Event::TurnStart { turn: 1 });
        drivers.observe(1, &root, &Event::TextDelta("checking now".into()));
        for _ in 0..2 {
            drivers.observe(
                1,
                &root,
                &Event::ToolQueued {
                    id: "call".into(),
                    name: "bash".into(),
                    args: serde_json::json!({}),
                },
            );
        }
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Progress("checking now".into())
            })
        );
        assert!(
            rx.try_recv().is_err(),
            "one progress message per completed text"
        );
        drivers.observe(
            1,
            &root,
            &Event::ToolProgress {
                id: "call".into(),
                text: "1/3".into(),
            },
        );
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Progress("1/3".into())
            })
        );
        drivers.observe(
            1,
            &root,
            &Event::ToolProgress {
                id: "call".into(),
                text: "2/3".into(),
            },
        );
        assert!(rx.try_recv().is_err(), "tool progress is paced");
        drivers.observe(1, &root, &Event::TurnStart { turn: 2 });
        drivers.observe(1, &root, &Event::TextDelta("discard this".into()));
        drivers.observe(
            1,
            &root,
            &Event::Retrying {
                attempt: 1,
                delay_ms: 0,
                reason: "cut".into(),
            },
        );
        drivers.observe(1, &root, &Event::TextDelta("done".into()));
        drivers.turn_ended(1, &Ended::Done);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Finished {
                    text: "done".into(),
                    outcome: Outcome::Done,
                }
            })
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn model_outcomes_are_separate_from_the_remaining_answer() {
        for (text, ended, outcome) in [
            ("stopped", Ended::Done, Outcome::Done),
            ("(stopped)", Ended::Done, Outcome::Done),
            (
                "running\nstopped ",
                Ended::Stopped,
                Outcome::Stopped(String::new()),
            ),
            ("", Ended::Stopped, Outcome::Stopped(String::new())),
            ("partial", Ended::Unsent, Outcome::Stopped(String::new())),
            (
                "partial",
                Ended::Failed("connection lost".into()),
                Outcome::Failed("connection lost".into()),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut drivers = Drivers::default();
            let execution = background::Commands::new(drivers.background()).start(
                dir.path().to_path_buf(),
                toolbox::background::Invocation::shell("bridge"),
                Default::default(),
            );
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            execution.channel().listen(tx);
            let request = Request::new(Origin::Input(execution.id()), 1);
            drivers.started(request, true);
            rx.try_recv().unwrap();
            drivers.observe(1, dir.path(), &Event::TextDelta(text.into()));
            drivers.turn_ended(1, &ended);
            assert_eq!(
                rx.try_recv(),
                Ok(Told::Response {
                    request: request.id,
                    event: Response::Finished {
                        text: text.into(),
                        outcome
                    },
                })
            );
        }
    }

    // The line a person sent through an execution opens a turn as if typed; the
    // execution hears it begin, then gets the whole answer, cut short or not.
    #[tokio::test]
    async fn a_turn_an_input_opened_is_told_back_to_its_command() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ctx = Ctx::new(tool::Workspace::new(&root).unwrap());
        let mut drivers = Drivers::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let execution = background::Commands::new(drivers.background()).start(
            root.clone(),
            toolbox::background::Invocation::shell("wechat"),
            Default::default(),
        );
        execution.channel().listen(tx);
        execution
            .channel()
            .input("why did it fail".into(), String::new());
        let next = drivers.next(&root, &ctx).expect("the input is due");
        assert_eq!(next.line, "why did it fail");
        assert_eq!(next.origin, Origin::Input(execution.id()));
        assert!(next.label.is_none(), "read as typed, not relayed");

        let request = Request::new(next.origin, 1);
        drivers.started(request, true);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Started
            })
        );
        drivers.observe(1, &root, &Event::TextDelta("parse.rs".into()));
        drivers.observe(2, &root, &Event::TextDelta("another lane".into()));
        drivers.turn_ended(1, &Ended::Stopped);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Finished {
                    text: "parse.rs".into(),
                    outcome: Outcome::Stopped(String::new()),
                }
            })
        );
        drivers.turn_ended(1, &Ended::Done);
        assert!(rx.try_recv().is_err(), "told once");
    }

    #[tokio::test]
    async fn queued_requests_only_emit_valid_state_changes() {
        let dir = tempfile::tempdir().unwrap();
        let drivers = Drivers::default();
        let execution = background::Commands::new(drivers.background()).start(
            dir.path().to_path_buf(),
            toolbox::background::Invocation::shell("bridge"),
            Default::default(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        execution.channel().listen(tx);
        let request = Request::new(Origin::Input(execution.id()), 1);
        for event in [
            Response::Queued,
            Response::Queued,
            Response::Progress("not started".into()),
            Response::Started,
            Response::Started,
            Response::Queued,
            Response::Progress("working".into()),
            Response::Finished {
                text: "answer".into(),
                outcome: Outcome::Done,
            },
            Response::Progress("already finished".into()),
            Response::Finished {
                text: "duplicate".into(),
                outcome: Outcome::Done,
            },
        ] {
            drivers.respond(request, event);
        }
        for event in [
            Response::Queued,
            Response::Started,
            Response::Progress("working".into()),
            Response::Finished {
                text: "answer".into(),
                outcome: Outcome::Done,
            },
        ] {
            assert_eq!(
                rx.try_recv(),
                Ok(Told::Response {
                    request: request.id,
                    event
                })
            );
        }
        let cancelled = Request::new(request.origin, 1);
        drivers.respond(cancelled, Response::Queued);
        let finished = Response::Finished {
            text: String::new(),
            outcome: Outcome::Stopped("withdrawn".into()),
        };
        drivers.respond(cancelled, finished.clone());
        drivers.respond(cancelled, finished.clone());
        for event in [Response::Queued, finished] {
            assert_eq!(
                rx.try_recv(),
                Ok(Told::Response {
                    request: cancelled.id,
                    event
                })
            );
        }
        assert!(rx.try_recv().is_err());
    }

    // The model is told of an execution the user stopped; one it could not stop
    // leaves it nothing to be told.
    #[tokio::test]
    async fn a_command_the_user_stops_is_noted_for_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let drivers = Drivers::default();
        let execution = background::Commands::new(drivers.background()).start(
            root.clone(),
            toolbox::background::Invocation::shell("http.server 8080"),
            Default::default(),
        );
        let Said::Noted(_, note) = drivers.stop_commands(&root, &[execution.id()]) else {
            panic!("a stop is noted")
        };
        assert!(
            note.contains(&format!("#{} (http.server 8080)", execution.id())),
            "{note}"
        );
        assert!(matches!(
            drivers.stop_commands(&root, &[execution.id()]),
            Said::Notice(_)
        ));
    }

    // A retried request streams its turn again: the execution hears the retry's
    // text once, and what earlier turns of the run said stays.
    #[tokio::test]
    async fn a_retried_turn_is_told_back_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ctx = Ctx::new(tool::Workspace::new(&root).unwrap());
        let mut drivers = Drivers::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let execution = background::Commands::new(drivers.background()).start(
            root.clone(),
            toolbox::background::Invocation::shell("wechat"),
            Default::default(),
        );
        execution.channel().listen(tx);
        execution.channel().input("btc".into(), String::new());
        let next = drivers.next(&root, &ctx).expect("the input is due");
        let request = Request::new(next.origin, 1);
        drivers.started(request, true);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Started
            })
        );

        let retrying = Event::Retrying {
            attempt: 1,
            delay_ms: 0,
            reason: "cut".into(),
        };
        for ev in [
            Event::TurnStart { turn: 1 },
            Event::TextDelta("checking. ".into()),
            Event::TurnStart { turn: 2 },
            Event::TextDelta("拿到".into()),
            retrying,
            Event::TextDelta("83057.6".into()),
        ] {
            drivers.observe(1, &root, &ev);
        }
        drivers.turn_ended(1, &Ended::Done);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Finished {
                    text: "checking. 83057.6".into(),
                    outcome: Outcome::Done,
                }
            })
        );
    }

    // A command that follows hears every run on its checkout from its next
    // request on — typed ones too — and its own input's run only once.
    #[tokio::test]
    async fn a_command_that_follows_is_told_every_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ctx = Ctx::new(tool::Workspace::new(&root).unwrap());
        let mut drivers = Drivers::default();
        let sink = background::Commands::new(drivers.background());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let execution = sink.start(
            root.clone(),
            toolbox::background::Invocation::shell("wechat"),
            Default::default(),
        );
        execution.channel().listen(tx);
        let (quiet_tx, mut quiet) = tokio::sync::mpsc::unbounded_channel();
        let _quiet = sink.start(
            root.clone(),
            toolbox::background::Invocation::shell("build"),
            Default::default(),
        );
        _quiet.channel().listen(quiet_tx);

        drivers.observe(1, &root, &Event::TurnStart { turn: 1 });
        drivers.observe(1, &root, &Event::TextDelta("before".into()));
        execution.channel().follow();
        drivers.observe(1, &root, &Event::TurnStart { turn: 2 });
        let Told::Response {
            request: first,
            event: Response::Started,
        } = rx.try_recv().unwrap()
        else {
            panic!("a followed run begins")
        };
        drivers.observe(1, &root, &Event::TextDelta("connected".into()));
        drivers.turn_ended(1, &Ended::Done);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: first,
                event: Response::Finished {
                    text: "connected".into(),
                    outcome: Outcome::Done,
                }
            })
        );
        assert!(
            quiet.try_recv().is_err(),
            "one that does not follow hears nothing"
        );

        execution.channel().input("btc".into(), String::new());
        let next = drivers.next(&root, &ctx).expect("the input is due");
        let request = Request::new(next.origin, 1);
        drivers.started(request, true);
        drivers.observe(1, &root, &Event::TurnStart { turn: 1 });
        drivers.observe(1, &root, &Event::TextDelta("83057.6".into()));
        drivers.turn_ended(1, &Ended::Done);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Started
            })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Finished {
                    text: "83057.6".into(),
                    outcome: Outcome::Done,
                }
            })
        );
        assert!(rx.try_recv().is_err(), "told once");
    }
}
