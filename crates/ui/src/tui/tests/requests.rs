use std::sync::Arc;
use std::time::Duration;

use pi_core::driver::{Ended, Origin};
use pi_core::execution::{Done, Kind, Request};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use toolbox::background::{RunHandle, RunSink};
use toolbox::channel::{Response, Told};

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
    let (done, done_rx) = unbounded_channel();
    let (fetched, _fetched_rx) = unbounded_channel();
    let (started, _started_rx) = unbounded_channel();
    match wake {
        Wake::Do(Asked::Core(intent), line) => {
            tui.execute(intent, origin, line, &done, &fetched, &started)
                .await;
        }
        Wake::Nothing => {}
        _ => panic!("a submission must be admitted or queued"),
    }
    done_rx
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
    assert_eq!(response(&mut rx), (id, Response::Finished(expected)));
    assert!(rx.try_recv().is_err());
    assert!(!tui.core.lane().is_running());

    submit(&mut tui, execution.as_ref(), "/there-is-no-such-command").await;
    let (id, _) = response(&mut rx);
    let (finished, Response::Finished(text)) = response(&mut rx) else {
        panic!("a final answer")
    };
    assert_eq!(finished, id);
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
        let (finished, Response::Finished(text)) = response(&mut rx) else {
            panic!("a final answer")
        };
        assert_eq!(finished, id);
        if line == "/compact" {
            assert!(text.contains("stop the run first"));
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
        (request.id, Response::Finished("the model's answer".into()))
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
    let (finished, Response::Finished(text)) = response(&mut rx) else {
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
                    Err(agent::AgentError::Cancelled),
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
        let (id, Response::Finished(text)) = response(&mut rx) else {
            panic!("a final answer")
        };
        assert_eq!(id, request.id);
        assert!(
            text.contains(match outcome {
                0 => "failed",
                1 => "stopped",
                _ => "compacted",
            }),
            "{text}"
        );
        assert!(rx.try_recv().is_err());
        assert!(tui.core.lane().is_running(), "the other lane was untouched");
    }
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
        (request.id, Response::Finished("done".into()))
    );
    tui.drivers
        .respond(request, Response::Finished("stale completion".into()));
    assert!(rx.try_recv().is_err(), "the response is completed once");
}

#[tokio::test]
async fn remote_prompts_and_shell_commands_keep_their_own_async_response() {
    for line in [" /compact is literal text", "!printf 'bridge output'"] {
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
        let (finished, Response::Finished(text)) = response(&mut rx) else {
            panic!("a final answer")
        };
        assert_eq!(finished, id);
        if line.starts_with('!') {
            assert_eq!(text, "bridge output");
        }
        assert!(
            rx.try_recv().is_err(),
            "following does not duplicate the input's own reply"
        );
        assert!(!tui.core.lane().is_running());
    }
}
