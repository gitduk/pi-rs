use std::sync::Arc;
use std::time::Duration;

use pi_core::driver::{Ended, Origin};
use pi_core::execution::{Done, Kind, Request, RunError};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use toolbox::background::{RunHandle, RunSink};
use toolbox::channel::{Outcome, Response, Told};

use super::harness::*;
use crate::tui::{Asked, Tui, Wake};

fn bridge(tui: &Tui) -> (Arc<dyn RunHandle>, UnboundedReceiver<Told>) {
    let execution = pi_core::driver::background::Commands::new(tui.drivers.background()).start(
        tui.core.lane().root().to_path_buf(),
        toolbox::background::Invocation::shell("bridge"),
        Default::default(),
    );
    let (tx, rx) = unbounded_channel();
    execution.channel().listen(tx);
    (execution, rx)
}

async fn submit(tui: &mut Tui, execution: &dyn RunHandle, text: &str) -> UnboundedReceiver<Done> {
    execution.channel().input(text.into());
    let (origin, wake) = tui.driver_line().expect("the submission was received");
    let (replies, inb) = crate::tui::run::Replies::channels();
    match wake {
        Wake::Do(Asked::Core(intent), line) => {
            tui.execute(intent, origin, line, &replies).await;
        }
        Wake::Nothing => {}
        _ => panic!("a submission must be admitted or queued"),
    }
    inb.done
}

fn response(rx: &mut UnboundedReceiver<Told>) -> (u64, Response) {
    let Told::Response { request, event } = rx.try_recv().expect("an operation response") else {
        panic!("an operation response has its own identity");
    };
    (request, event)
}

#[tokio::test]
async fn remote_commands_return_the_same_content_that_the_local_surface_displays() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    tui.core.lane_mut().finish();
    let pi_core::input::Step::Handled(rows) = tui.core.dispatch(pi_core::input::Intent::Builtin(
        pi_core::input::Builtin::Status,
    )) else {
        panic!("status returns a listing")
    };
    let expected = crate::listing::lines(&rows).join("\n");
    let (execution, mut rx) = bridge(&tui);
    submit(&mut tui, execution.as_ref(), "/status").await;
    let (id, event) = response(&mut rx);
    assert_eq!(event, Response::Started);
    assert_eq!(
        response(&mut rx),
        (
            id,
            Response::Finished {
                text: expected,
                outcome: Outcome::Done
            }
        )
    );
    assert!(rx.try_recv().is_err());
    assert!(!tui.core.lane().is_running());

    submit(&mut tui, execution.as_ref(), "/there-is-no-such-command").await;
    let (id, _) = response(&mut rx);
    let (finished, Response::Finished { text, outcome }) = response(&mut rx) else {
        panic!("a final answer")
    };
    assert_eq!(finished, id);
    assert_eq!(outcome, Outcome::Done);
    assert!(text.contains("unknown command"));
    assert!(
        !tui.core.lane().is_running(),
        "a typo never becomes a model prompt"
    );
}

#[tokio::test]
async fn a_status_request_and_a_refused_compact_do_not_end_the_running_models_response() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    execution.channel().follow();
    let request = Request::new(Origin::Typed, tui.core.lane().token());
    tui.drivers.started(request, true);
    tui.drivers.observe(
        request.lane,
        dir.path(),
        &agent::Event::TurnStart { turn: 1 },
    );
    assert_eq!(response(&mut rx), (request.id, Response::Started));

    for line in ["/status", "/compact"] {
        submit(&mut tui, execution.as_ref(), line).await;
        let (id, event) = response(&mut rx);
        assert_ne!(id, request.id);
        assert_eq!(event, Response::Started);
        let (finished, Response::Finished { text, outcome }) = response(&mut rx) else {
            panic!("a final answer")
        };
        assert_eq!(finished, id);
        if line == "/compact" {
            assert!(text.is_empty());
            let Outcome::Failed(why) = outcome else {
                panic!("a refused command has a failed outcome")
            };
            assert!(why.contains("stop the run first"));
        } else {
            assert_eq!(outcome, Outcome::Done);
            assert!(!text.is_empty());
        }
        assert!(tui.core.lane().is_running());
        assert!(rx.try_recv().is_err());
    }
    tui.drivers.observe(
        request.lane,
        dir.path(),
        &agent::Event::TextDelta("the model's answer".into()),
    );
    tui.drivers
        .turn_ended(request.lane, &Ended::Done, tui.core.lane().ctx(), None);
    assert_eq!(
        response(&mut rx),
        (
            request.id,
            Response::Finished {
                text: "the model's answer".into(),
                outcome: Outcome::Done,
            }
        )
    );
}

#[tokio::test]
async fn a_compact_with_nothing_to_shrink_still_finishes_its_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    tui.core
        .lane_mut()
        .return_session(agent::session::Session::default());
    tui.core.lane_mut().finish();
    let (execution, mut rx) = bridge(&tui);
    let mut done = submit(&mut tui, execution.as_ref(), "/compact").await;
    let (id, event) = response(&mut rx);
    assert_eq!(event, Response::Started);
    let completed = tokio::time::timeout(Duration::from_secs(2), done.recv())
        .await
        .unwrap()
        .unwrap();
    tui.settle(completed).await;
    let (
        finished,
        Response::Finished {
            text,
            outcome: Outcome::Done,
        },
    ) = response(&mut rx)
    else {
        panic!("a final answer")
    };
    assert_eq!(finished, id);
    assert!(text.contains("nothing to compact"));
    assert!(!tui.core.lane().is_running());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn compact_failure_cancellation_and_success_survive_a_lane_switch() {
    for outcome in [0, 1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut tui = surface(dir.path());
        let (execution, mut rx) = bridge(&tui);
        let request = Request::new(Origin::Input(execution.id()), tui.core.lane().token());
        tui.drivers.started(request, false);
        assert_eq!(response(&mut rx), (request.id, Response::Started));
        switch_to(&mut tui, running_lane(other.path()));
        let (report, ran) = match outcome {
            0 => (None, None),
            1 => (
                None,
                Some((
                    agent::session::Session::default(),
                    Err(agent::AgentError::Cancelled.into()),
                )),
            ),
            _ => (
                Some(agent::Report::default()),
                Some((agent::session::Session::default(), Ok(Default::default()))),
            ),
        };
        tui.settle(Done {
            request,
            kind: Kind::Compact(report),
            ran,
        })
        .await;
        let (
            id,
            Response::Finished {
                text,
                outcome: ended,
            },
        ) = response(&mut rx)
        else {
            panic!("a final answer")
        };
        assert_eq!(id, request.id);
        match (outcome, ended) {
            (0, Outcome::Failed(why)) => {
                assert!(text.is_empty());
                assert!(why.contains("the run crashed"));
            }
            (1, Outcome::Stopped(reason)) => {
                assert!(text.is_empty());
                assert!(reason.is_empty());
            }
            (2, Outcome::Done) => assert!(text.contains("compacted")),
            (_, ended) => panic!("unexpected outcome: {ended:?}"),
        }
        assert!(rx.try_recv().is_err());
        assert!(tui.core.lane().is_running(), "the other lane was untouched");
    }
}

#[tokio::test]
async fn command_reply_text_does_not_determine_whether_the_operation_stopped() {
    for outcome in [
        Outcome::Done,
        Outcome::Stopped(String::new()),
        Outcome::Failed("denied".into()),
        Outcome::Exited(3),
    ] {
        for notice in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut tui = surface(dir.path());
            let (execution, mut rx) = bridge(&tui);
            let request = Request::new(Origin::Input(execution.id()), tui.core.lane().token());
            tui.drivers.started(request, false);
            assert_eq!(response(&mut rx), (request.id, Response::Started));
            let expected = Response::Finished {
                text: "stopped".into(),
                outcome: outcome.clone(),
            };
            let output = if notice {
                pi_core::execution::Output::Notice("stopped".into())
            } else {
                pi_core::execution::Output::Text("stopped".into())
            };
            tui.complete(request, output, outcome.clone());
            assert_eq!(response(&mut rx), (request.id, expected));
            tui.drivers.respond(
                request,
                Response::Finished {
                    text: "duplicate".into(),
                    outcome: Outcome::Stopped(String::new()),
                },
            );
            assert!(rx.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn diagnostic_notices_and_empty_completions_have_no_reply_content() {
    for outcome in [
        Outcome::Done,
        Outcome::Stopped(String::new()),
        Outcome::Stopped("run it again".into()),
        Outcome::Failed("denied".into()),
        Outcome::Exited(3),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut tui = surface(dir.path());
        let (execution, mut rx) = bridge(&tui);
        let request = Request::new(Origin::Input(execution.id()), tui.core.lane().token());
        tui.drivers.started(request, false);
        response(&mut rx);
        tui.complete(
            request,
            pi_core::execution::Output::Notice(String::new()),
            outcome.clone(),
        );
        assert_eq!(
            response(&mut rx),
            (
                request.id,
                Response::Finished {
                    text: String::new(),
                    outcome,
                }
            )
        );
    }
}

#[tokio::test]
async fn named_tool_failure_separates_diagnostics_before_any_rendering() {
    let dir = tempfile::tempdir().unwrap();
    let tui = surface(dir.path());
    let request = Request::new(Origin::Typed, tui.core.lane().token());
    let completion = pi_core::execution::tool(
        request,
        toolbox::builtin().get("read").unwrap(),
        serde_json::json!({"path": "missing.txt"}),
        tui.core.lane().ctx().clone(),
    )
    .await;
    let Outcome::Failed(why) = completion.outcome else {
        panic!("a failed tool has a diagnostic outcome")
    };
    assert!(why.contains("missing.txt"), "{why}");
    let pi_core::execution::Output::Notice(text) = completion.output else {
        panic!("the failed tool has no content to display")
    };
    assert!(text.is_empty());
}

#[tokio::test]
async fn bash_cancellation_and_refusal_do_not_become_reply_lines() {
    for refused in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut tui = surface(dir.path());
        let (execution, mut rx) = bridge(&tui);
        let request = Request::new(Origin::Input(execution.id()), tui.core.lane().token());
        tui.drivers.started(request, false);
        response(&mut rx);
        let (lines, ran, expected) = if refused {
            (
                Vec::new(),
                Err(RunError::Failed("permission denied".into())),
                Outcome::Failed("permission denied".into()),
            )
        } else {
            (
                vec!["actual output".into(), "stopped".into()],
                Err(agent::AgentError::Cancelled.into()),
                Outcome::Stopped(String::new()),
            )
        };
        let text = lines.join("\n");
        tui.settle(Done {
            request,
            kind: Kind::Bash { lines },
            ran: Some((agent::session::Session::default(), ran)),
        })
        .await;
        assert_eq!(
            response(&mut rx),
            (
                request.id,
                Response::Finished {
                    text,
                    outcome: expected
                }
            )
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    let request = Request::new(Origin::Input(execution.id()), tui.core.lane().token());
    tui.drivers.started(request, false);
    response(&mut rx);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let done = pi_core::execution::bash(
        request,
        agent::session::Session::default(),
        tui.core.lane().ctx_for(cancel),
        "printf should-not-run".into(),
        toolbox::bash::Bash::foreground(),
    )
    .await;
    tui.settle(done).await;
    assert_eq!(
        response(&mut rx),
        (
            request.id,
            Response::Finished {
                text: String::new(),
                outcome: Outcome::Stopped(String::new()),
            }
        )
    );
}

#[tokio::test]
async fn background_results_remain_model_input_while_remote_commands_are_admitted() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    execution.channel().result("/compact".into());
    submit(&mut tui, execution.as_ref(), "/status").await;
    response(&mut rx);
    response(&mut rx);
    let due = tui
        .drivers
        .background()
        .take_ready_results(dir.path())
        .expect("the result was left for the model");
    assert!(due.line.ends_with("/compact"));
    assert!(tui.core.lane().is_running());
}

#[tokio::test]
async fn shell_exit_status_is_separate_from_actual_output() {
    for (command, expected) in [("!printf 'exit 3'; exit 3", "exit 3"), ("!exit 3", "")] {
        let dir = tempfile::tempdir().unwrap();
        let mut tui = surface(dir.path());
        tui.core
            .lane_mut()
            .return_session(agent::session::Session::default());
        tui.core.lane_mut().finish();
        let (execution, mut rx) = bridge(&tui);
        let mut done = submit(&mut tui, execution.as_ref(), command).await;
        let (id, _) = response(&mut rx);
        let completed = tokio::time::timeout(Duration::from_secs(3), done.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(completed.ran.as_ref().unwrap().1.is_err());
        assert!(matches!(completed.ended(), Ended::Failed(_)));
        tui.settle(completed).await;
        assert_eq!(
            response(&mut rx),
            (
                id,
                Response::Finished {
                    text: expected.into(),
                    outcome: Outcome::Exited(3),
                }
            )
        );
    }
}

#[tokio::test]
async fn moving_from_prompt_fetch_to_model_execution_begins_and_ends_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    execution.channel().follow();
    let request = Request::new(Origin::Input(execution.id()), tui.core.lane().token());
    tui.drivers.started(request, false);
    assert_eq!(response(&mut rx), (request.id, Response::Started));
    tui.drivers.started(request, true);
    tui.drivers.observe(
        request.lane,
        dir.path(),
        &agent::Event::TurnStart { turn: 1 },
    );
    assert!(
        rx.try_recv().is_err(),
        "fetching and executing belong to one operation"
    );
    tui.drivers.observe(
        request.lane,
        dir.path(),
        &agent::Event::TextDelta("done".into()),
    );
    tui.drivers
        .turn_ended(request.lane, &Ended::Done, tui.core.lane().ctx(), None);
    assert_eq!(
        response(&mut rx),
        (
            request.id,
            Response::Finished {
                text: "done".into(),
                outcome: Outcome::Done
            }
        )
    );
    tui.drivers.respond(
        request,
        Response::Finished {
            text: "stale completion".into(),
            outcome: Outcome::Done,
        },
    );
    assert!(rx.try_recv().is_err(), "the response is completed once");
}

#[tokio::test]
async fn queued_prompts_and_shell_commands_keep_one_identity_until_completion() {
    for line in ["a queued prompt", "!printf 'queued output'", "!true"] {
        let dir = tempfile::tempdir().unwrap();
        let mut tui = surface(dir.path());
        tui.core
            .lane_mut()
            .return_session(agent::session::Session::default());
        let (execution, mut rx) = bridge(&tui);
        execution.channel().follow();
        let pending = submit(&mut tui, execution.as_ref(), line).await;
        let (id, event) = response(&mut rx);
        assert_eq!(event, Response::Queued);
        assert!(
            rx.try_recv().is_err(),
            "the queued operation has not started"
        );
        assert!(pending.is_closed(), "no task was spawned");
        let queued = crate::tui::front_view(&mut tui.views, tui.core.lane())
            .queued
            .pop_front()
            .unwrap();
        assert_eq!(queued.request.id, id);
        tui.core.lane_mut().finish();
        let (replies, inb) = crate::tui::run::Replies::channels();
        let mut completed = inb.done;
        assert!(!tui.resume_queued(queued, &replies).await);
        assert_eq!(response(&mut rx), (id, Response::Started));
        let done = tokio::time::timeout(Duration::from_secs(3), completed.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.request.id, id);
        tui.settle(done).await;
        let (finished, Response::Finished { text, outcome }) = response(&mut rx) else {
            panic!("a final reply")
        };
        assert_eq!(finished, id);
        assert_eq!(
            outcome,
            if line.starts_with('!') {
                Outcome::Exited(0)
            } else {
                Outcome::Done
            }
        );
        assert_eq!(
            text,
            if line.contains("printf") {
                "queued output"
            } else {
                ""
            }
        );
        assert!(
            rx.try_recv().is_err(),
            "following does not duplicate lifecycle events"
        );
    }
}

#[tokio::test]
async fn a_queued_input_that_cannot_start_still_finishes_with_its_original_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    submit(&mut tui, execution.as_ref(), "a prompt").await;
    let (id, event) = response(&mut rx);
    assert_eq!(event, Response::Queued);
    let queued = crate::tui::front_view(&mut tui.views, tui.core.lane())
        .queued
        .pop_front()
        .unwrap();
    tui.core.lane_mut().finish();
    let (replies, inb) = crate::tui::run::Replies::channels();
    let mut completed = inb.done;
    assert!(!tui.resume_queued(queued, &replies).await);
    assert_eq!(response(&mut rx), (id, Response::Started));
    let (
        finished,
        Response::Finished {
            text,
            outcome: Outcome::Failed(why),
        },
    ) = response(&mut rx)
    else {
        panic!("the request is failed even though no task started")
    };
    assert_eq!(finished, id);
    assert!(text.is_empty());
    assert!(why.contains("no transcript"));
    assert!(completed.try_recv().is_err());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn recalling_queued_inputs_finishes_them_without_stopping_the_active_run() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    let mut ids = Vec::new();
    for line in ["first", "!true"] {
        submit(&mut tui, execution.as_ref(), line).await;
        let (id, event) = response(&mut rx);
        assert_eq!(event, Response::Queued);
        ids.push(id);
    }
    tui.ui.editor.set_line("draft");
    let asked = tui.ui.key(
        tui.core.lane(),
        crate::tui::front_view(&mut tui.views, tui.core.lane()),
        crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        true,
    );
    let crate::tui::Asked::Own(deed) = asked else {
        panic!("recall is a local action")
    };
    tui.carry(deed).await;
    assert_eq!(tui.ui.editor.text(), "first\n!true\ndraft");
    assert!(
        crate::tui::front_view(&mut tui.views, tui.core.lane())
            .queued
            .is_empty()
    );
    assert!(tui.core.lane().is_running());
    for id in ids {
        assert_eq!(
            response(&mut rx),
            (
                id,
                Response::Finished {
                    text: String::new(),
                    outcome: Outcome::Stopped("recalled for editing".into()),
                }
            )
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "cancelled queued inputs never start"
    );
}

#[tokio::test]
async fn shutdown_terminates_waiting_requests_before_stopping_their_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    submit(&mut tui, execution.as_ref(), "!true").await;
    let (id, event) = response(&mut rx);
    assert_eq!(event, Response::Queued);
    tui.core.lane_mut().finish();
    let (_done, mut pending) = unbounded_channel();
    tui.settle_all(&mut pending, "shutdown").await;
    assert_eq!(
        response(&mut rx),
        (
            id,
            Response::Finished {
                text: String::new(),
                outcome: Outcome::Stopped("shutdown".into()),
            }
        )
    );
    assert!(rx.try_recv().is_err());
    assert!(
        crate::tui::front_view(&mut tui.views, tui.core.lane())
            .queued
            .is_empty()
    );
}

#[tokio::test]
async fn removing_a_lane_terminates_its_queued_requests() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let (execution, mut rx) = bridge(&tui);
    submit(&mut tui, execution.as_ref(), "!true").await;
    let (id, event) = response(&mut rx);
    assert_eq!(event, Response::Queued);
    let (done, _) = unbounded_channel();
    tui.deliver_prompt(
        crate::tui::clipboard::prompt("new direction".into(), None, &[]),
        Origin::Typed,
        Some("new direction".into()),
        &done,
    );
    let queue = crate::tui::front_view(&mut tui.views, tui.core.lane())
        .queued
        .clone();
    let pending: Vec<_> = queue.lock().iter().map(|q| q.request).collect();
    assert_eq!(pending.len(), 2);
    switch_to(&mut tui, running_lane(other.path()));
    tui.core.lanes.remove(0);
    tui.core.current = 0;
    let removed = crate::tui::view::prune_views(&tui.core, &mut tui.views);
    assert_eq!(
        removed, pending,
        "both queues are cancelled with their lane"
    );
    assert!(queue.is_empty());
    for request in removed {
        tui.cancel_queued(request, "its session was removed");
    }
    assert_eq!(
        response(&mut rx),
        (
            id,
            Response::Finished {
                text: String::new(),
                outcome: Outcome::Stopped("its session was removed".into()),
            }
        )
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn remote_prompts_and_shell_commands_keep_their_own_async_response() {
    for line in [
        " /compact is literal text",
        "!printf 'bridge output'",
        "!true",
        "!printf '(no output)'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut tui = surface(dir.path());
        tui.core
            .lane_mut()
            .return_session(agent::session::Session::default());
        tui.core.lane_mut().finish();
        let (execution, mut rx) = bridge(&tui);
        execution.channel().follow();
        let mut done = submit(&mut tui, execution.as_ref(), line).await;
        let (id, event) = response(&mut rx);
        assert_eq!(event, Response::Started);
        let completed = tokio::time::timeout(Duration::from_secs(3), done.recv())
            .await
            .unwrap()
            .unwrap();
        if line.starts_with(' ') {
            let (session, _) = completed
                .ran
                .as_ref()
                .expect("the model returned its session");
            assert_eq!(session.last_prompt().unwrap().text, line);
        }
        tui.settle(completed).await;
        let (finished, Response::Finished { text, outcome }) = response(&mut rx) else {
            panic!("a final answer")
        };
        assert_eq!(finished, id);
        assert_eq!(
            outcome,
            if line.starts_with('!') {
                Outcome::Exited(0)
            } else {
                Outcome::Done
            }
        );
        match line {
            "!printf 'bridge output'" => assert_eq!(text, "bridge output"),
            "!true" => assert!(text.is_empty()),
            "!printf '(no output)'" => assert_eq!(text, "(no output)"),
            _ => {}
        }
        assert!(
            rx.try_recv().is_err(),
            "following does not duplicate the input's own reply"
        );
        assert!(!tui.core.lane().is_running());
    }
}
