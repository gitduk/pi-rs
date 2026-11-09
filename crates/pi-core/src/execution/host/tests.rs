use super::*;
use crate::core::lane::Opening;
use crate::driver::Origin;
use crate::execution::messages::Messages;
use agent::session::{Prompt, Session};
use llm::stream::{BlockKind, StopReason, StreamEvent, Usage};
use std::path::Path;
use tokio::sync::Notify;

#[derive(Default)]
struct Wire {
    entered: Notify,
    release: Notify,
    panic: bool,
}

#[async_trait::async_trait]
impl llm::Transport for Wire {
    async fn stream(
        &self,
        _: &llm::model::ModelSpec,
        _: &llm::request::Request,
    ) -> llm::Result<futures::stream::BoxStream<'static, llm::Result<StreamEvent>>> {
        self.entered.notify_one();
        self.release.notified().await;
        assert!(!self.panic, "fixture transport panic");
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                delta: "complete answer".into(),
            }),
            Ok(StreamEvent::Done {
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        ])))
    }
}

fn lane(root: &Path, wire: Arc<Wire>) -> Lane {
    let spec = llm::model::ModelSpec {
        model: "fixture".into(),
        wire_id: None,
        base_url: "http://localhost".into(),
        headers: vec![],
        session_header: None,
        format: llm::model::Format::Anthropic {
            cache_control: llm::model::CacheControl::Off,
        },
        context_window: 200_000,
        max_output_tokens: 8000,
        vision: false,
        accepts_effort: true,
        accepts_temperature: true,
        can_force_tool: true,
        pricing: Default::default(),
    };
    let id = pi_store::session::new_id();
    let mut resolved = crate::core::lane::a_resolved("");
    Arc::make_mut(&mut resolved).watched =
        crate::core::resolve::watched(&crate::args::Pinned::default(), root);
    let mut lane = Lane::opened(Opening {
        id: id.clone(),
        ..Opening::new(
            Arc::new(agent::Agent::new(wire, spec)),
            resolved,
            tool::Ctx::new(tool::Workspace::new(root).unwrap())
                .with_session(id, root.join("spill")),
        )
    });
    lane.return_session(Session::new());
    lane
}

fn prompt() -> Prompt {
    Prompt {
        text: "work".into(),
        images: vec![],
        shown: None,
        relayed: None,
    }
}

#[tokio::test]
async fn a_model_run_saves_without_a_frontend_receiving_its_completion() {
    let dir = tempfile::tempdir().unwrap();
    let store = pi_store::session::Store::new(dir.path().join("state"));
    let wire = Arc::new(Wire::default());
    let mut lane = lane(dir.path(), wire.clone());
    let input = lane.messages().admit(Origin::Typed, prompt()).unwrap();
    let request_id = input.message().request_id().unwrap();
    let request = Request::new(Origin::Typed, lane.token());
    let opened = prepare(
        &mut lane,
        input,
        request,
        &store,
        Arc::default(),
        agent::Retry::default(),
    )
    .unwrap();
    assert_eq!(opened.run.message().request_id().unwrap(), request_id);
    assert!(lane.is_running());
    assert!(lane.session().is_none());
    let task = tokio::spawn(opened.run.run());
    wire.entered.notified().await;
    lane.set_name(Some("renamed during execution".into()));
    wire.release.notify_one();
    let done = task.await.unwrap();
    assert_eq!(done.request, request);
    assert!(matches!(done.ended(), Ended::Done));
    let path = store.path_of(lane.root(), lane.id());
    let stored = pi_store::session::Store::read(&path).unwrap();
    assert_eq!(stored.name.as_deref(), Some("renamed during execution"));
    assert!(
        stored
            .into_session()
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Answer { .. }))
    );
    let restored = Messages::load(
        lane.messages().session(),
        &pi_store::session::messages::path(&path),
    )
    .unwrap();
    let response = restored.response(&request_id).unwrap().unwrap();
    assert_eq!(
        response.body(),
        &[message::Block::Text("complete answer".into())]
    );
    let (session, _) = done.ran.unwrap();
    lane.return_session(session);
    lane.finish();
}

#[tokio::test]
async fn cancellation_is_saved_before_the_model_run_returns() {
    let dir = tempfile::tempdir().unwrap();
    let store = pi_store::session::Store::new(dir.path().join("state"));
    let wire = Arc::new(Wire::default());
    let mut lane = lane(dir.path(), wire.clone());
    let input = lane.messages().admit(Origin::Typed, prompt()).unwrap();
    let id = input.message().request_id().unwrap();
    let request = Request::new(Origin::Typed, lane.token());
    let opened = prepare(
        &mut lane,
        input,
        request,
        &store,
        Arc::default(),
        agent::Retry::default(),
    )
    .unwrap();
    let task = tokio::spawn(opened.run.run());
    wire.entered.notified().await;
    lane.cancel();
    let done = task.await.unwrap();
    assert!(matches!(done.ended(), Ended::Stopped));
    let path = store.path_of(lane.root(), lane.id());
    let restored = Messages::load(
        lane.messages().session(),
        &pi_store::session::messages::path(&path),
    )
    .unwrap();
    assert!(matches!(
        restored.response(&id).unwrap().unwrap().kind(),
        message::Kind::Response {
            status: message::Status::Stopped,
            ..
        }
    ));
}

#[test]
fn readiness_never_skips_a_host_operation_or_the_original_input_order() {
    use crate::execution::queue::{Pending, Queue, Queued};
    let messages = Messages::new(message::SessionId::new("ordered".into()).unwrap());
    let first = messages.admit(Origin::Typed, prompt()).unwrap();
    let second = messages.admit(Origin::Typed, prompt()).unwrap();
    let queue = Queue::default();
    let request = Request::new(Origin::Typed, 1);
    queue.push(Queued {
        input: Pending::Prompt(second),
        request,
        line: None,
        note: String::new(),
    });
    assert!(
        queue.pop_ready().is_none(),
        "a later ticket cannot pass its predecessor"
    );
    let first_id = first.message().id().clone();
    queue.push(Queued {
        input: Pending::Prompt(first),
        request,
        line: None,
        note: String::new(),
    });
    let first = queue.pop_ready().unwrap();
    assert!(matches!(&first.input,Pending::Prompt(input) if input.message().id() == &first_id));
    assert!(queue.pop_ready().is_none());
    drop(first);
    assert!(queue.pop_ready().is_some());
    queue.push(Queued {
        input: Pending::Intent(crate::input::Intent::Builtin(crate::input::Builtin::Status)),
        request,
        line: None,
        note: String::new(),
    });
    assert!(queue.pop_ready().is_none());
    assert_eq!(queue.lock().len(), 1);
}

#[test]
fn readiness_covers_every_open_session_and_skips_only_busy_sessions() {
    use crate::execution::queue::{Pending, Queued};
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut lanes = vec![
        lane(a.path(), Arc::default()),
        lane(b.path(), Arc::default()),
    ];
    for lane in &lanes {
        let input = lane.messages().admit(Origin::Typed, prompt()).unwrap();
        lane.queue().push(Queued {
            input: Pending::Prompt(input),
            request: Request::new(Origin::Typed, lane.token()),
            line: None,
            note: String::new(),
        });
    }
    lanes[0].begin(tokio_util::sync::CancellationToken::new());
    let selected = ready(&lanes);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].request.lane, lanes[1].token());
    assert_eq!(lanes[0].queue().lock().len(), 1);
    lanes[0].finish();
    assert_eq!(ready(&lanes)[0].request.lane, lanes[0].token());
}

#[tokio::test]
async fn a_crashed_model_keeps_the_native_archive_and_saves_its_terminal_message() {
    let dir = tempfile::tempdir().unwrap();
    let store = pi_store::session::Store::new(dir.path().join("state"));
    let wire = Arc::new(Wire {
        panic: true,
        ..Default::default()
    });
    let mut lane = lane(dir.path(), wire.clone());
    let original = Session::new();
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
            &original,
        )
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let input = lane.messages().admit(Origin::Typed, prompt()).unwrap();
    let id = input.message().request_id().unwrap();
    let request = Request::new(Origin::Typed, lane.token());
    let opened = prepare(
        &mut lane,
        input,
        request,
        &store,
        Arc::default(),
        agent::Retry::default(),
    )
    .unwrap();
    let task = tokio::spawn(opened.run.run());
    wire.entered.notified().await;
    wire.release.notify_one();
    let done = task.await.unwrap();
    assert!(done.ran.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let restored = Messages::load(
        lane.messages().session(),
        &pi_store::session::messages::path(&path),
    )
    .unwrap();
    assert!(matches!(
        restored.response(&id).unwrap().unwrap().kind(),
        message::Kind::Response {
            status: message::Status::Failed(_),
            ..
        }
    ));
}

mod runtime;
