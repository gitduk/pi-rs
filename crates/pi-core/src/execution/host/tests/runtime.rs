use super::*;
use crate::core::{Core, lanes::Lanes};
use crate::driver::background::Commands;
use crate::execution::{Kind, RunError, queue::Pending};
use pi_tools::background::{Invocation, Owner, RunHandle, RunSink, Scope};

fn core(lane: Lane, store: pi_store::session::Store, table: Arc<Table>) -> Core {
    Core {
        runtime: Default::default(),
        commands: lane.resolved().commands.clone(),
        store,
        catalog: Default::default(),
        config: Arc::new(pi_store::config::Config::default()),
        pinned: Default::default(),
        prompts_seen: 0,
        settings: pi_store::settings::Settings::new(toml::Value::Table(Default::default()), None),
        lanes: Lanes::of(lane),
        refused: Default::default(),
        executions: Some(table),
        surface: &[],
        removals: Default::default(),
    }
}

fn source(table: &Arc<Table>, lane: &Lane) -> Arc<dyn RunHandle> {
    source_after(table, lane, None)
}

#[tokio::test]
async fn a_hidden_queued_input_gets_drift_without_replacing_front_configuration() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let path = a.path().join("changed.txt");
    std::fs::write(&path, "after\n").unwrap();
    let wire = Arc::new(Wire::default());
    let table = Arc::new(Table::default());
    let hidden = lane(a.path(), wire.clone());
    hidden.ctx().note_view(&path, "before\n");
    let mut core = core(
        hidden,
        pi_store::session::Store::new(a.path().join("state")),
        table.clone(),
    );
    let execution = source(&table, &core.lanes[0]);
    execution.channel().result("ready".into());
    execution.end();
    core.lanes.push_ahead(lane(b.path(), Arc::default()));
    let config = core.config.clone();
    let commands = core.commands.clone();
    let super::super::runtime::Event::Started(started) = core.next_runtime_event().await else {
        panic!()
    };
    assert_eq!(started.request.lane, core.lanes[0].token());
    assert!(started.entries.iter().any(|e| matches!(e, Entry::Note { note, .. } if note.contains("changed.txt") && note.contains("after"))));
    assert_eq!(core.lanes.front(), 1);
    assert!(Arc::ptr_eq(&core.config, &config));
    assert!(Arc::ptr_eq(&core.commands, &commands));
    wire.release.notify_one();
    while !matches!(
        core.next_runtime_event().await,
        super::super::runtime::Event::Settled(_)
    ) {}
    assert!(
        core.lanes[0]
            .session()
            .unwrap()
            .entries()
            .iter()
            .any(|e| matches!(e, Entry::Note { note, .. } if note.contains("changed.txt")))
    );
}

fn source_after(
    table: &Arc<Table>,
    lane: &Lane,
    request: Option<message::MessageId>,
) -> Arc<dyn RunHandle> {
    Commands::new(table.clone()).start(
        Scope {
            root: lane.root().into(),
            owner: Some(Owner {
                session: lane.id().into(),
                request,
            }),
        },
        Invocation::shell("source"),
        Default::default(),
    )
}

#[tokio::test]
async fn a_hidden_sessions_result_runs_and_returns_to_core_without_a_frontend() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let wire = Arc::new(Wire::default());
    let table = Arc::new(Table::default());
    let hidden = lane(a.path(), wire.clone());
    let token = hidden.token();
    let mut core = core(
        hidden,
        pi_store::session::Store::new(a.path().join("state")),
        table.clone(),
    );
    core.lanes.push_ahead(lane(b.path(), Arc::default()));
    let (_, mut parent) = core.lanes[0]
        .messages()
        .admit(Origin::Typed, prompt())
        .unwrap()
        .start()
        .unwrap();
    let cause = parent.request().id().clone();
    parent.complete(message::Status::Done, vec![]).unwrap();
    let execution = source_after(&table, &core.lanes[0], Some(cause.clone()));
    execution.channel().result("compile finished".into());
    execution.end();
    assert!(core.admit_background_results().is_empty());
    assert!(core.lane().queue().is_empty());
    let queued = core.take_ready_inputs().pop().unwrap();
    assert_eq!(queued.request.lane, token);
    let Pending::Prompt(input) = queued.input else {
        panic!()
    };
    assert!(matches!(
        input.message().kind(),
        message::Kind::Request(message::Request::ToModel(message::InputKind::Automatic))
    ));
    assert!(input.prompt().text.contains("compile finished"));
    assert_eq!(input.message().by(), Some(&cause));
    let id = input.message().request_id().unwrap();
    let opened = prepare(
        core.lanes.at_mut(0),
        input,
        queued.request,
        &core.store,
        table.clone(),
        Default::default(),
    )
    .unwrap();
    core.lanes
        .at_mut(0)
        .push_plain("retained while the run owns the transcript");
    wire.release.notify_one();
    let done = opened.run.run().await;
    let Settlement::Returned(settled) = core.settle_execution(done) else {
        panic!()
    };
    assert_eq!(settled.lane, 0);
    assert_eq!(settled.request.lane, token);
    assert!(settled.result.is_ok());
    assert!(matches!(settled.recovery, Recovery::Unneeded));
    assert!(!core.lanes[0].is_running());
    assert!(core.lanes[0].session().is_some());
    crate::core::saves::flush().await;
    let path = core.store.path_of(core.lanes[0].root(), core.lanes[0].id());
    let saved = pi_store::session::Store::read(&path)
        .unwrap()
        .into_session();
    assert!(saved.entries().iter().any(|e| matches!(e,Entry::Plain {text,..} if text == "retained while the run owns the transcript")));
    let messages = Messages::load(
        core.lanes[0].messages().session(),
        &pi_store::session::messages::path(&path),
    )
    .unwrap();
    assert!(messages.response(&id).unwrap().is_some());
    assert!(core.admit_background_results().is_empty());
    assert!(
        core.take_ready_inputs().is_empty(),
        "a completion creates one automatic input"
    );
}

#[test]
fn rejected_background_admission_keeps_the_result_for_a_later_retry() {
    let dir = tempfile::tempdir().unwrap();
    let lane = lane(dir.path(), Arc::default());
    let table = Arc::new(Table::default());
    let execution = source(&table, &lane);
    execution.channel().result("keep this result".into());
    execution.end();
    let core = core(
        lane,
        pi_store::session::Store::new(dir.path().join("state")),
        table.clone(),
    );
    let delivery = table.claim_result(dir.path(), core.lane().id()).unwrap();
    let event = delivery.message(core.lane().messages().session()).id;
    drop(delivery);
    let mut tickets = Vec::new();
    for _ in 0..256 {
        tickets.push(
            core.lane()
                .messages()
                .admit(Origin::Typed, prompt())
                .unwrap(),
        );
    }
    let refused = core.admit_background_results();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].lane, core.lane().token());
    assert!(
        core.admit_background_results().is_empty(),
        "unchanged refusal is reported once"
    );
    assert!(core.lane().queue().is_empty());
    assert!(
        table.take_back(dir.path()).is_none(),
        "another consumer cannot bypass deferred admission"
    );
    assert!(
        table.next_due(dir.path(), Some(core.lane().id())).is_none(),
        "a deferred result cannot cause an immediate wake loop"
    );
    drop(tickets);
    assert!(core.admit_background_results().is_empty());
    let queued = core.take_ready_inputs().pop().unwrap();
    let Pending::Prompt(input) = queued.input else {
        panic!()
    };
    assert!(input.prompt().text.contains("keep this result"));
    assert_eq!(
        input.message().id(),
        &event,
        "retry retains the original completion identity"
    );
    assert!(table.take_back(dir.path()).is_none());
}

#[test]
fn pending_inputs_and_active_runs_keep_priority_over_background_completion() {
    let dir = tempfile::tempdir().unwrap();
    let lane = lane(dir.path(), Arc::default());
    let table = Arc::new(Table::default());
    let execution = source(&table, &lane);
    execution.channel().result("later result".into());
    execution.end();
    let mut core = core(
        lane,
        pi_store::session::Store::new(dir.path().join("state")),
        table.clone(),
    );
    let input = core
        .lane()
        .messages()
        .admit(Origin::Typed, prompt())
        .unwrap();
    core.lane().queue().push(crate::execution::queue::Queued {
        input: Pending::Prompt(input),
        request: Request::new(Origin::Typed, core.lane().token()),
        line: None,
        note: String::new(),
    });
    core.admit_background_results();
    assert_eq!(core.lane().queue().lock().len(), 1);
    drop(core.take_ready_inputs());
    core.lane_mut().begin(Default::default());
    core.admit_background_results();
    assert!(core.lane().queue().is_empty());
    core.lane_mut().finish();
    core.admit_background_results();
    assert_eq!(core.lane().queue().lock().len(), 1);
}

#[tokio::test]
async fn recovery_and_failure_are_settled_by_core_before_the_next_input() {
    let dir = tempfile::tempdir().unwrap();
    let store = pi_store::session::Store::new(dir.path().join("state"));
    let mut lane = lane(dir.path(), Arc::default());
    let archive = lane.session().unwrap().clone();
    let path = store
        .save(
            lane.root(),
            pi_store::session::Header {
                id: lane.id(),
                model: lane.model(),
                name: None,
                created: 0,
                spent: Default::default(),
            },
            &archive,
        )
        .unwrap();
    lane.take_session();
    lane.begin(Default::default());
    let request = Request::new(Origin::Typed, lane.token());
    let mut core = core(lane, store, Arc::default());
    let Settlement::Returned(settled) = core.settle_execution(Done {
        request,
        kind: Kind::Turn,
        ran: None,
    }) else {
        panic!()
    };
    assert!(matches!(settled.recovery, Recovery::Restored));
    assert!(matches!(settled.result, Err(RunError::Crashed)));
    assert!(!core.lane().is_running());
    assert_eq!(core.lane().session().unwrap(), &archive);
    crate::core::saves::flush().await;
    assert!(pi_store::session::Store::read(&path).is_ok());
}

#[tokio::test]
async fn stale_completion_cannot_return_a_transcript_owned_by_another_operation() {
    let dir = tempfile::tempdir().unwrap();
    let mut lane = lane(dir.path(), Arc::default());
    let old = Request::new(Origin::Typed, lane.token());
    let active = Request::new(Origin::Typed, lane.token());
    lane.take_session();
    let cancel = tokio_util::sync::CancellationToken::new();
    lane.begin_for(active, cancel.clone());
    let mut core = core(
        lane,
        pi_store::session::Store::new(dir.path().join("state")),
        Arc::default(),
    );
    assert!(matches!(
        core.settle_execution(Done {
            request: old,
            kind: Kind::Turn,
            ran: Some((Session::new(), Ok(Default::default())))
        }),
        Settlement::Ignored
    ));
    assert!(core.lane().is_running());
    assert!(core.lane().session().is_none());
    assert!(!cancel.is_cancelled());
    assert!(matches!(
        core.settle_execution(Done {
            request: active,
            kind: Kind::Turn,
            ran: Some((Session::new(), Ok(Default::default())))
        }),
        Settlement::Returned(_)
    ));
    assert!(!core.lane().is_running());
    assert!(core.lane().session().is_some());
    crate::core::saves::flush().await;
}

#[test]
fn equal_completion_text_from_two_occurrences_opens_two_distinct_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let lane = lane(dir.path(), Arc::default());
    let table = Arc::new(Table::default());
    let execution = source(&table, &lane);
    execution.channel().result("same text".into());
    execution.channel().result("same text".into());
    let core = core(
        lane,
        pi_store::session::Store::new(dir.path().join("state")),
        table.clone(),
    );
    core.admit_background_results();
    let first = core.take_ready_inputs().pop().unwrap();
    let Pending::Prompt(ref first_input) = first.input else {
        panic!()
    };
    let first_id = first_input.message().id().clone();
    core.admit_background_results();
    assert!(
        core.take_ready_inputs().is_empty(),
        "an outstanding ticket retains execution order"
    );
    // The second input can be admitted, but cannot run until the first ends.
    drop(first);
    let second = core.take_ready_inputs().pop().unwrap();
    let Pending::Prompt(second) = second.input else {
        panic!()
    };
    assert_ne!(second.message().id(), &first_id);
    assert!(second.prompt().text.contains("same text"));
}

#[test]
fn a_pending_recall_blocks_automatic_admission_until_it_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let lane = lane(dir.path(), Arc::default());
    let table = Arc::new(Table::default());
    let execution = source(&table, &lane);
    execution.channel().result("after recall".into());
    execution.end();
    let mut core = core(
        lane,
        pi_store::session::Store::new(dir.path().join("state")),
        table,
    );
    core.lane_mut().end(false, true);
    core.admit_background_results();
    assert!(core.take_ready_inputs().is_empty());
    assert_eq!(core.lane_mut().take_ended(), Some(true));
    core.admit_background_results();
    assert_eq!(core.take_ready_inputs().len(), 1);
}

#[tokio::test]
async fn a_background_bash_exit_becomes_one_automatic_input_caused_by_the_tool_call() {
    use tool::Tool as _;
    let dir = tempfile::tempdir().unwrap();
    let wire = Arc::new(Wire::default());
    let table = Arc::new(Table::default());
    let mut core = core(
        lane(dir.path(), wire.clone()),
        pi_store::session::Store::new(dir.path().join("state")),
        table.clone(),
    );
    let messages = core.lane().messages();
    let (_, mut parent) = messages
        .admit(Origin::Typed, prompt())
        .unwrap()
        .start()
        .unwrap();
    let args =
        serde_json::json!({"background":true,"command":"printf 'background fixture result'"});
    let mut tool = messages
        .child(
            &parent.request().request_id().unwrap(),
            message::WireMessage {
                id: crate::execution::messages::new_id(),
                session: messages.session(),
                by: Some(parent.request().id().clone()),
                at: 1,
                kind: message::WireKind::Request(message::Request::FromModel(
                    message::ModelRequest::Tool {
                        name: "bash".into(),
                    },
                )),
                body: vec![message::Block::Data(args.clone())],
            },
            Some("native-bash".into()),
        )
        .unwrap();
    let cause = tool.request().id().clone();
    let context = core.lane().ctx().clone().with_request(Some(tool.request()));
    let shell = pi_tools::bash::Bash::new(Arc::new(Commands::new(table.clone())));
    let launched = shell.execute(args, &context).await.unwrap();
    tool.complete(
        message::Status::Done,
        vec![message::Block::Text(launched.flatten())],
    )
    .unwrap();
    parent.complete(message::Status::Done, vec![]).unwrap();
    let queued = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            assert!(core.admit_background_results().is_empty());
            if let Some(input) = core.take_ready_inputs().pop() {
                break input;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let Pending::Prompt(input) = queued.input else {
        panic!()
    };
    assert_eq!(input.message().by(), Some(&cause));
    assert!(input.prompt().text.contains("background fixture result"));
    let id = input.message().request_id().unwrap();
    let opened = prepare(
        core.lane_mut(),
        input,
        queued.request,
        &pi_store::session::Store::new(dir.path().join("state")),
        table.clone(),
        Default::default(),
    )
    .unwrap();
    wire.release.notify_one();
    let done = opened.run.run().await;
    assert!(matches!(
        core.settle_execution(done),
        Settlement::Returned(_)
    ));
    assert!(messages.response(&id).unwrap().is_some());
    core.admit_background_results();
    assert!(core.take_ready_inputs().is_empty());
    crate::core::saves::flush().await;
}

#[tokio::test]
async fn the_host_loop_runs_multiple_results_without_frontend_scheduling() {
    use crate::execution::host::Event;
    let dir = tempfile::tempdir().unwrap();
    let wire = Arc::new(Wire::default());
    let table = Arc::new(Table::default());
    let mut core = core(
        lane(dir.path(), wire.clone()),
        pi_store::session::Store::new(dir.path().join("state")),
        table.clone(),
    );
    let execution = source(&table, core.lane());
    execution.channel().result("first occurrence".into());
    execution.channel().result("second occurrence".into());
    execution.end();
    let mut order = Vec::new();
    let mut messages = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match core.next_runtime_event().await {
                Event::Started(started) => {
                    order.push("started");
                    messages.push(started.message);
                    wire.release.notify_one();
                }
                Event::Agent {
                    event: agent::Event::TextDelta(_),
                    ..
                } => order.push("text"),
                Event::Settled(Settlement::Returned(settled)) => {
                    assert!(settled.result.is_ok());
                    assert!(core.lane().session().is_some());
                    order.push("settled");
                    if messages.len() == 2 {
                        break;
                    }
                }
                Event::Deferred(_) | Event::Rejected { .. } => panic!("valid input was refused"),
                Event::Activity
                | Event::Agent { .. }
                | Event::Settled(_)
                | Event::Stopping { .. } => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        order,
        ["started", "text", "settled", "started", "text", "settled"]
    );
    assert_ne!(messages[0].id(), messages[1].id());
    assert!(!core.runtime_busy());
    for message in messages {
        assert!(
            core.lane()
                .messages()
                .response(&message.request_id().unwrap())
                .unwrap()
                .is_some()
        );
    }
    crate::core::saves::flush().await;
}

#[tokio::test]
async fn cancelling_an_event_wait_keeps_the_running_task_and_its_original_response() {
    use crate::execution::host::Event;
    let dir = tempfile::tempdir().unwrap();
    let wire = Arc::new(Wire::default());
    let mut core = core(
        lane(dir.path(), wire.clone()),
        pi_store::session::Store::new(dir.path().join("state")),
        Arc::default(),
    );
    let input = core
        .lane()
        .messages()
        .admit(Origin::Typed, prompt())
        .unwrap();
    let id = input.message().request_id().unwrap();
    let request = Request::new(Origin::Typed, core.lane().token());
    core.start_model(input, request).unwrap();
    wire.entered.notified().await;
    let mut cancelled = false;
    for _ in 0..32 {
        if tokio::time::timeout(
            std::time::Duration::from_millis(1),
            core.next_runtime_event(),
        )
        .await
        .is_err()
        {
            cancelled = true;
            break;
        }
    }
    assert!(cancelled);
    assert!(core.runtime_busy());
    assert!(core.lane().session().is_none());
    wire.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Event::Settled(Settlement::Returned(settled)) = core.next_runtime_event().await {
                assert_eq!(settled.request, request);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!core.runtime_busy());
    assert!(core.lane().messages().response(&id).unwrap().is_some());
    crate::core::saves::flush().await;
}

#[tokio::test]
async fn shutdown_stops_admission_before_returning_the_active_execution() {
    use crate::execution::{host::Event, queue::Queued};
    let dir = tempfile::tempdir().unwrap();
    let wire = Arc::new(Wire::default());
    let mut core = core(
        lane(dir.path(), wire.clone()),
        pi_store::session::Store::new(dir.path().join("state")),
        Arc::default(),
    );
    let input = core
        .lane()
        .messages()
        .admit(Origin::Typed, prompt())
        .unwrap();
    let request = Request::new(Origin::Typed, core.lane().token());
    core.start_model(input, request).unwrap();
    wire.entered.notified().await;
    let queued = core
        .lane()
        .messages()
        .admit(Origin::Typed, prompt())
        .unwrap();
    let id = queued.message().request_id().unwrap();
    let queued_request = Request::new(Origin::Typed, core.lane().token());
    core.lane().queue().push(Queued {
        input: Pending::Prompt(queued),
        request: queued_request,
        line: None,
        note: String::new(),
    });
    assert_eq!(core.stop_runtime(), vec![queued_request]);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while core.runtime_busy() {
            match core.next_runtime_event().await {
                Event::Started(_) => panic!("shutdown launched queued work"),
                Event::Settled(Settlement::Returned(settled)) => {
                    assert!(matches!(settled.ended, Ended::Stopped))
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(core.lane().session().is_some());
    assert!(matches!(
        core.lane()
            .messages()
            .response(&id)
            .unwrap()
            .unwrap()
            .kind(),
        message::Kind::Response {
            status: message::Status::Stopped,
            ..
        }
    ));
    assert!(core.lane().queue().is_empty());
    crate::core::saves::flush().await;
}

#[tokio::test]
async fn shutdown_closes_new_inputs_but_keeps_observation_available_until_ports_stop() {
    let dir = tempfile::tempdir().unwrap();
    let table = Arc::new(Table::default());
    let mut core = core(
        lane(dir.path(), Arc::default()),
        pi_store::session::Store::new(dir.path().join("state")),
        table.clone(),
    );
    core.enable_background(table.clone());
    let bridge = source(&table, core.lane());
    let cursor = core.lane().messages().cursor();
    core.stop_runtime();
    let wire = message::WireMessage {
        id: crate::execution::messages::new_id(),
        session: core.lane().messages().session(),
        by: None,
        at: 1,
        kind: message::WireKind::Request(message::Request::ToModel(message::InputKind::User)),
        body: vec![message::Block::Text("too late".into())],
    };
    assert!(
        bridge
            .channel()
            .message(wire)
            .unwrap_err()
            .contains("no longer accepting")
    );
    assert_eq!(core.lane().messages().cursor(), cursor);
    assert!(
        bridge
            .channel()
            .observe(core.lane().messages().session(), None)
            .is_ok()
    );
    core.finish_runtime_shutdown("host test shutdown");
    assert!(!table.working(dir.path()));
    let path = core.store.path_of(dir.path(), core.lane().id());
    let stored = pi_store::session::Store::read(&path)
        .unwrap()
        .into_session();
    assert!(
        stored.entries().iter().any(
            |entry| matches!(entry,Entry::Note {note,..} if note.contains("host test shutdown"))
        )
    );
    crate::core::saves::flush().await;
}

#[tokio::test]
async fn startup_restores_the_original_message_ids_before_starting_another_round() {
    use crate::execution::host::Event;
    let dir = tempfile::tempdir().unwrap();
    let store = pi_store::session::Store::new(dir.path().join("state"));
    let wire = Arc::new(Wire::default());
    let mut first = core(
        lane(dir.path(), wire.clone()),
        store.clone(),
        Arc::default(),
    );
    first.restore_messages().unwrap();
    let input = first
        .lane()
        .messages()
        .admit(Origin::Typed, prompt())
        .unwrap();
    let original = input.message().request_id().unwrap();
    let request = Request::new(Origin::Typed, first.lane().token());
    first.start_model(input, request).unwrap();
    wire.release.notify_one();
    while first.runtime_busy() {
        first.next_runtime_event().await;
    }
    crate::core::saves::flush().await;
    let response = first
        .lane()
        .messages()
        .response(&original)
        .unwrap()
        .unwrap();
    let id = first.lane().id().to_string();
    let agent = first.lane().agent().clone();
    let resolved = first.lane().resolved().clone();
    let ctx = first.lane().ctx().clone();
    drop(first);
    let stored = pi_store::session::Store::read(&store.path_of(dir.path(), &id)).unwrap();
    let mut resumed = Lane::opened(Opening {
        id,
        ..Opening::new(Arc::new(agent), resolved, ctx)
    });
    resumed.return_session(stored.into_session());
    let mut resumed = core(resumed, store.clone(), Arc::default());
    resumed.restore_messages().unwrap();
    assert_eq!(
        resumed
            .lane()
            .messages()
            .response(&original)
            .unwrap()
            .unwrap(),
        response
    );
    let input = resumed
        .lane()
        .messages()
        .admit(Origin::Typed, prompt())
        .unwrap();
    let second = input.message().request_id().unwrap();
    let request = Request::new(Origin::Typed, resumed.lane().token());
    resumed.start_model(input, request).unwrap();
    wire.release.notify_one();
    while resumed.runtime_busy() {
        if let Event::Settled(Settlement::Returned(settled)) = resumed.next_runtime_event().await {
            assert!(settled.result.is_ok());
        }
    }
    crate::core::saves::flush().await;
    assert_eq!(
        resumed
            .lane()
            .messages()
            .response(&original)
            .unwrap()
            .unwrap(),
        response
    );
    assert!(
        resumed
            .lane()
            .messages()
            .response(&second)
            .unwrap()
            .is_some()
    );
    assert!(resumed.lane().unsaved().is_none());
}

#[tokio::test]
async fn a_stream_drain_does_not_overtake_queued_execution_starts() {
    use crate::execution::host::Event;
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let table = Arc::new(Table::default());
    let mut core = core(
        lane(a.path(), Arc::default()),
        pi_store::session::Store::new(a.path().join("state")),
        table.clone(),
    );
    core.lanes.push_ahead(lane(b.path(), Arc::default()));
    for lane in core.lanes.all() {
        let source = source(&table, lane);
        source.channel().result("ready".into());
        source.end();
    }
    assert!(matches!(core.next_runtime_event().await, Event::Started(_)));
    core.lanes[1]
        .sender()
        .send(agent::Event::TextDelta("already generated".into()))
        .unwrap();
    assert!(
        core.try_agent_event().is_none(),
        "start presentation must precede buffered output"
    );
    assert!(matches!(core.next_runtime_event().await, Event::Started(_)));
    assert!(core.try_agent_event().is_some());
    core.stop_runtime();
    while core.runtime_busy() {
        core.next_runtime_event().await;
    }
    crate::core::saves::flush().await;
}
