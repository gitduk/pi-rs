use super::*;
use crate::driver::background::{Commands, Table};
use crate::execution::messages::Messages;
use agent::seams::Input as _;

#[tokio::test]
async fn free_text_answers_the_same_request_without_parsing_commands() {
    let dir = tempfile::tempdir().unwrap();
    let table = Arc::new(Table::default());
    let tool = AskUser(Arc::new(Commands::new(table.clone())));
    let args = json!({ "text": "Which branch?" });
    let (kind, body) = tool.model_request(&args);
    assert!(matches!(&kind, ModelRequest::Ask { options } if options.is_empty()));
    let messages = Messages::new(message::SessionId::new("ask-session".into()).unwrap());
    let prompt = agent::session::Prompt {
        text: "work".into(),
        images: vec![],
        shown: None,
        relayed: None,
    };
    let (_, mut owner) = messages
        .admit(crate::driver::Origin::Typed, prompt)
        .unwrap()
        .start()
        .unwrap();
    let input = crate::driver::input::Input::new(dir.path().into(), table.clone(), None)
        .with_messages(messages.clone(), owner.request().request_id().unwrap());
    let call = llm::message::ToolCall {
        id: "call-ask".into(),
        name: "ask".into(),
        args: args.clone(),
    };
    let request = input.tool_request(&call, kind, body).unwrap().unwrap();
    let ctx =
        Ctx::new(tool::Workspace::new(dir.path()).unwrap()).with_request(Some(request.clone()));
    let task = tokio::spawn(async move { tool.execute(args, &ctx).await });
    let question = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(question) = table.questions(dir.path()).first() {
                break question.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(question.request.as_ref(), Some(request.id()));
    table.answer(question.id, Some(" ".into()));
    assert_eq!(table.questions(dir.path()).len(), 1);
    table.answer(question.id, Some("/new".into()));
    table.answer(question.id, Some("late answer".into()));
    let accepted = messages
        .response(&request.request_id().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(accepted.body(), &[Block::Text("/new".into())]);
    let before = messages.cursor();
    let result = task.await.unwrap().unwrap();
    assert!(
        matches!(&result.content[..], [llm::message::ToolResultContent::Text(text)] if text.text == "/new")
    );
    input
        .tool_response(&llm::message::ToolResult {
            call: call.id,
            name: call.name,
            content: result.content,
            is_error: false,
        })
        .unwrap();
    assert_eq!(
        messages.cursor(),
        before,
        "native retention does not publish a second user response"
    );
    owner.complete(message::Status::Done, vec![]).unwrap();
    assert!(table.questions(dir.path()).is_empty());
}

#[tokio::test]
async fn cancelled_input_releases_its_pending_display() {
    let dir = tempfile::tempdir().unwrap();
    let table = Arc::new(Table::default());
    let tool = AskUser(Arc::new(Commands::new(table.clone())));
    let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
    let cancel = ctx.cancel.clone();
    let task = tokio::spawn(async move {
        tool.execute(json!({"text": "Continue?", "options": ["yes", "no"]}), &ctx)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while table.questions(dir.path()).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(matches!(task.await.unwrap(), Err(ToolError::Cancelled)));
    assert!(table.questions(dir.path()).is_empty());
}

struct Setup {
    dir: tempfile::TempDir,
    table: Arc<Table>,
    messages: Messages,
    input: crate::driver::input::Input,
    owner: crate::execution::messages::Responder,
    channel: Arc<dyn ::execution::background::RunHandle>,
    _binding: Arc<crate::execution::ingress::Ingress>,
}

impl Setup {
    fn new() -> Self {
        use ::execution::background::RunSink as _;
        let dir = tempfile::tempdir().unwrap();
        let table = Arc::new(Table::default());
        let messages = Messages::new(message::SessionId::new("user-response".into()).unwrap());
        let (_, owner) = messages
            .admit(
                crate::driver::Origin::Typed,
                agent::session::Prompt {
                    text: "work".into(),
                    images: vec![],
                    shown: None,
                    relayed: None,
                },
            )
            .unwrap()
            .start()
            .unwrap();
        let input = crate::driver::input::Input::new(dir.path().into(), table.clone(), None)
            .with_messages(messages.clone(), owner.request().request_id().unwrap());
        let binding =
            table.bind_inputs(dir.path().into(), 17, messages.clone(), Default::default());
        let channel = Commands::new(table.clone()).start(
            ::execution::background::Scope {
                root: dir.path().into(),
                owner: Some(::execution::background::Owner {
                    session: messages.session().as_str().into(),
                    request: None,
                }),
            },
            ::execution::background::Invocation::shell("channel"),
            Default::default(),
        );
        Self {
            dir,
            table,
            messages,
            input,
            owner,
            channel,
            _binding: binding,
        }
    }
    fn ctx(&self) -> Ctx {
        Ctx::new(tool::Workspace::new(self.dir.path()).unwrap()).with_session(
            self.messages.session().as_str(),
            self.dir.path().join("spill"),
        )
    }
    fn response(&self, request: &message::Message) -> message::WireMessage {
        message::WireMessage {
            id: crate::execution::messages::new_id(),
            session: self.messages.session(),
            by: Some(request.id().clone()),
            at: 2,
            kind: message::WireKind::Response {
                request: request.id().clone(),
                status: message::Status::Done,
            },
            body: vec![Block::Text("/new".into())],
        }
    }
}

#[tokio::test]
async fn explicit_answer_can_arrive_before_display_and_keeps_its_original_message() {
    let mut setup = Setup::new();
    let tool = AskUser(Arc::new(Commands::new(setup.table.clone())));
    let args = json!({"text":"Which?", "options":["yes", "no"]});
    let call = llm::message::ToolCall {
        id: "native-ask".into(),
        name: "ask".into(),
        args: args.clone(),
    };
    let (kind, body) = tool.model_request(&args);
    let request = setup
        .input
        .tool_request(&call, kind, body)
        .unwrap()
        .unwrap();
    let answer = setup.response(&request);
    let mut invalid = answer.clone();
    invalid.body = vec![Block::Data(json!({"picks":[0]}))];
    assert!(setup.channel.channel().message(invalid).is_err());
    assert!(
        setup
            .messages
            .response(&request.request_id().unwrap())
            .unwrap()
            .is_none()
    );
    let accepted = setup
        .channel
        .channel()
        .message(answer.clone())
        .unwrap()
        .response
        .unwrap();
    assert_eq!(accepted.id(), &answer.id);
    let before = setup.messages.cursor();
    let ctx = setup.ctx().with_request(Some(request.clone()));
    let output = tool.execute(args, &ctx).await.unwrap();
    assert!(
        setup.table.questions(setup.dir.path()).is_empty(),
        "an already answered request does not open another prompt"
    );
    assert_eq!(llm::message::flatten(&output.content), "/new");
    let result = setup.input.completed_calls().unwrap().pop().unwrap();
    assert_eq!(result.call, "native-ask");
    assert_eq!(result.name, "ask");
    setup.input.tool_response(&result).unwrap();
    assert_eq!(setup.messages.cursor(), before);
    assert_eq!(
        setup
            .channel
            .channel()
            .message(answer.clone())
            .unwrap()
            .response
            .unwrap(),
        accepted
    );
    let mut conflict = answer;
    conflict.body = vec![Block::Text("different".into())];
    assert!(setup.channel.channel().message(conflict).is_err());
    assert!(
        setup._binding.queue.is_empty(),
        "answering does not open another model turn"
    );
    setup.owner.complete(message::Status::Done, vec![]).unwrap();
}

struct AskWire;
#[async_trait]
impl llm::Transport for AskWire {
    async fn stream(
        &self,
        _: &llm::model::ModelSpec,
        _: &llm::request::Request,
    ) -> llm::Result<futures::stream::BoxStream<'static, llm::Result<llm::stream::StreamEvent>>>
    {
        use futures::StreamExt as _;
        use llm::stream::{BlockKind, StopReason, StreamEvent, Usage};
        Ok(futures::stream::iter(
            vec![
                StreamEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::ToolCall {
                        id: Some("native-ask".into()),
                        name: "ask".into(),
                    },
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: r#"{"text":"Which?","options":["yes","no"]}"#.into(),
                },
                StreamEvent::Done {
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                },
            ]
            .into_iter()
            .map(Ok),
        )
        .boxed())
    }
}

#[tokio::test]
async fn accepted_answer_survives_cancellation_and_native_history_keeps_images() {
    for answer_first in [true, false] {
        let mut setup = Setup::new();
        let mut agent =
            agent::Agent::new(Arc::new(AskWire), crate::core::tests::test_spec("model"));
        Arc::make_mut(&mut agent.brief).registry =
            tool::Registry::new().with(AskUser(Arc::new(Commands::new(setup.table.clone()))));
        let mut session = agent::session::Session::with_prompt("work");
        let ctx = setup.ctx();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let retry = agent::Retry::default();
        let mut run =
            Box::pin(agent.run_with_input(&mut session, &ctx, &events, &retry, &setup.input));
        assert!(futures::poll!(run.as_mut()).is_pending());
        let question = setup.table.questions(setup.dir.path())[0].clone();
        let request = setup
            .messages
            .request(question.request.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let mut answer = setup.response(&request);
        let image = message::Image::Url {
            url: "https://example.com/answer.png".into(),
        };
        answer.body = vec![Block::Text("confirmed".into()), Block::Image(image.clone())];
        let accepted = if answer_first {
            Some(
                setup
                    .channel
                    .channel()
                    .message(answer.clone())
                    .unwrap()
                    .response
                    .unwrap(),
            )
        } else {
            None
        };
        ctx.cancel.cancel();
        assert!(matches!(run.await, Err(agent::AgentError::Cancelled)));
        assert!(setup.table.questions(setup.dir.path()).is_empty());
        let response = setup
            .messages
            .response(&request.request_id().unwrap())
            .unwrap()
            .unwrap();
        let native: Vec<_> = session
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                agent::session::Entry::Tool { result, .. } if result.call == "native-ask" => {
                    Some(result)
                }
                _ => None,
            })
            .collect();
        assert_eq!(native.len(), 1);
        if let Some(accepted) = accepted {
            assert_eq!(response, accepted);
            assert!(!native[0].is_error);
            assert_eq!(
                native[0].content,
                vec![
                    llm::message::ToolResultContent::Text(llm::message::Text {
                        text: "confirmed".into()
                    }),
                    llm::message::ToolResultContent::Image(image)
                ]
            );
            assert_eq!(
                setup
                    .channel
                    .channel()
                    .message(answer)
                    .unwrap()
                    .response
                    .unwrap(),
                accepted
            );
        } else {
            assert!(matches!(
                response.kind(),
                message::Kind::Response {
                    status: message::Status::Stopped,
                    ..
                }
            ));
            assert!(native[0].is_error);
            assert!(setup.channel.channel().message(answer).is_err());
        }
        setup.input.close_calls(message::Status::Stopped).unwrap();
        setup
            .owner
            .complete(message::Status::Stopped, vec![])
            .unwrap();
    }
}

#[tokio::test]
async fn dropping_the_native_owner_closes_the_user_port_and_its_parent() {
    let setup = Setup::new();
    let tool = AskUser(Arc::new(Commands::new(setup.table.clone())));
    let args = json!({"text":"Continue?"});
    let (kind, body) = tool.model_request(&args);
    let call = llm::message::ToolCall {
        id: "native-ask".into(),
        name: "ask".into(),
        args: args.clone(),
    };
    let request = setup
        .input
        .tool_request(&call, kind, body)
        .unwrap()
        .unwrap();
    let root = setup.owner.request().request_id().unwrap();
    let ctx = setup.ctx().with_request(Some(request.clone()));
    let mut run = Box::pin(tool.execute(args, &ctx));
    assert!(futures::poll!(run.as_mut()).is_pending());
    drop(setup.owner);
    drop(setup.input);
    assert!(matches!(run.await, Err(ToolError::Cancelled)));
    assert!(setup.table.questions(setup.dir.path()).is_empty());
    for id in [request.request_id().unwrap(), root] {
        assert!(matches!(
            setup.messages.response(&id).unwrap().unwrap().kind(),
            message::Kind::Response {
                status: message::Status::Failed(_),
                ..
            }
        ));
    }
}

#[tokio::test]
async fn concurrent_channels_commit_one_answer_and_retries_survive_reload() {
    use ::execution::background::RunSink as _;
    let mut setup = Setup::new();
    let tool = AskUser(Arc::new(Commands::new(setup.table.clone())));
    let args = json!({"text":"Choose"});
    let call = llm::message::ToolCall {
        id: "native-ask".into(),
        name: "ask".into(),
        args: args.clone(),
    };
    let (kind, body) = tool.model_request(&args);
    let request = setup
        .input
        .tool_request(&call, kind, body)
        .unwrap()
        .unwrap();
    let left = setup.response(&request);
    let mut right = setup.response(&request);
    right.body = vec![Block::Text("right".into())];
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let submit = |wire: message::WireMessage| {
        let channel = setup.channel.clone();
        let barrier = barrier.clone();
        tokio::task::spawn_blocking(move || {
            barrier.wait();
            channel.channel().message(wire)
        })
    };
    let (a, b) = tokio::join!(submit(left.clone()), submit(right.clone()));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let winner = if a.is_ok() { left } else { right };
    let response = setup
        .messages
        .response(&request.request_id().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(response.id(), &winner.id);
    setup.input.close_calls(message::Status::Stopped).unwrap();
    setup.owner.complete(message::Status::Done, vec![]).unwrap();
    let archive = pi_store::session::Location::new(
        setup.dir.path().join("project.db"),
        setup.messages.session(),
    );
    setup.messages.snapshot().save(&archive).unwrap();
    let restored = Messages::load(setup.messages.session(), &archive).unwrap();
    let table = Arc::new(Table::default());
    let _binding = table.bind_inputs(
        setup.dir.path().into(),
        18,
        restored.clone(),
        Default::default(),
    );
    let channel = Commands::new(table).start(
        ::execution::background::Scope {
            root: setup.dir.path().into(),
            owner: Some(::execution::background::Owner {
                session: restored.session().as_str().into(),
                request: None,
            }),
        },
        ::execution::background::Invocation::shell("reconnected"),
        Default::default(),
    );
    let before = restored.cursor();
    assert_eq!(
        channel.channel().message(winner).unwrap().response.unwrap(),
        response
    );
    assert_eq!(
        restored.cursor(),
        before,
        "replay cannot append or re-execute a response"
    );
}

#[test]
fn a_user_connection_cannot_return_tool_or_model_results() {
    let mut setup = Setup::new();
    let original = setup.owner.request();
    assert!(
        setup
            .channel
            .channel()
            .message(setup.response(&original))
            .is_err()
    );
    assert!(
        setup
            .messages
            .response(&original.request_id().unwrap())
            .unwrap()
            .is_none()
    );
    let wire = message::WireMessage {
        id: crate::execution::messages::new_id(),
        session: setup.messages.session(),
        by: Some(original.id().clone()),
        at: 2,
        kind: message::WireKind::Request(message::Request::FromModel(ModelRequest::Tool {
            name: "read".into(),
        })),
        body: vec![Block::Data(json!({}))],
    };
    let mut port = setup
        .messages
        .child(
            &original.request_id().unwrap(),
            wire,
            Some("native-tool".into()),
        )
        .unwrap();
    let request = port.request();
    assert!(
        setup
            .channel
            .channel()
            .message(setup.response(&request))
            .is_err()
    );
    assert!(
        setup
            .messages
            .response(&request.request_id().unwrap())
            .unwrap()
            .is_none()
    );
    port.complete(message::Status::Done, vec![]).unwrap();
    setup.owner.complete(message::Status::Done, vec![]).unwrap();
}

#[derive(Default)]
struct FinishingWire(std::sync::Mutex<Vec<llm::request::Request>>);

#[async_trait]
impl llm::Transport for FinishingWire {
    async fn stream(
        &self,
        spec: &llm::model::ModelSpec,
        request: &llm::request::Request,
    ) -> llm::Result<futures::stream::BoxStream<'static, llm::Result<llm::stream::StreamEvent>>>
    {
        use futures::StreamExt as _;
        use llm::stream::{BlockKind, StopReason, StreamEvent, Usage};
        let first = {
            let mut requests = self.0.lock().unwrap();
            let first = requests.is_empty();
            requests.push(request.clone());
            first
        };
        if first {
            return AskWire.stream(spec, request).await;
        }
        Ok(futures::stream::iter(vec![
            Ok(StreamEvent::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                delta: "done".into(),
            }),
            Ok(StreamEvent::Done {
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        ])
        .boxed())
    }
}

struct Annotate;
#[async_trait]
impl agent::Hooks for Annotate {
    async fn after(&self, _: &str, _: &Value, output: &ToolOutput, _: &Ctx) -> Option<String> {
        assert_eq!(output.flatten(), "user answer");
        Some("hook context".into())
    }
}

#[tokio::test]
async fn native_projection_preserves_hook_context_and_failure_does_not_cancel_the_run() {
    for failed in [false, true] {
        let mut setup = Setup::new();
        let wire = Arc::new(FinishingWire::default());
        let mut agent = agent::Agent::new(wire.clone(), crate::core::tests::test_spec("model"));
        Arc::make_mut(&mut agent.brief).registry =
            tool::Registry::new().with(AskUser(Arc::new(Commands::new(setup.table.clone()))));
        if !failed {
            Arc::make_mut(&mut agent.brief).hooks = Arc::new(Annotate);
        }
        let mut session = agent::session::Session::with_prompt("work");
        let ctx = setup.ctx();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let retry = agent::Retry::default();
        let mut run =
            Box::pin(agent.run_with_input(&mut session, &ctx, &events, &retry, &setup.input));
        assert!(futures::poll!(run.as_mut()).is_pending());
        let question = setup.table.questions(setup.dir.path())[0].clone();
        let request = setup
            .messages
            .request(question.request.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let mut answer = setup.response(&request);
        answer.body = vec![Block::Text(
            if failed { "partial" } else { "user answer" }.into(),
        )];
        if failed {
            answer.kind = message::WireKind::Response {
                request: request.id().clone(),
                status: message::Status::Failed(message::Failure {
                    code: "DELIVERY_FAILED".into(),
                    message: "channel unavailable".into(),
                }),
            };
            answer.body.push(Block::Image(message::Image::Url {
                url: "https://example.com/partial.png".into(),
            }));
        }
        let response = setup
            .channel
            .channel()
            .message(answer)
            .unwrap()
            .response
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), run)
            .await
            .unwrap()
            .unwrap();
        let requests = wire.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let native: Vec<_> = requests[1]
            .messages
            .iter()
            .filter_map(|message| match message {
                llm::message::Message::User { content } => Some(content),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                llm::message::UserContent::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(native.len(), 1);
        assert_eq!(native[0].call, "native-ask");
        assert_eq!(native[0].is_error, failed);
        let text = llm::message::flatten(&native[0].content);
        if failed {
            assert!(
                text.contains("partial")
                    && text.contains("DELIVERY_FAILED")
                    && text.contains("channel unavailable")
            );
            assert!(
                native[0]
                    .content
                    .iter()
                    .any(|block| matches!(block, llm::message::ToolResultContent::Image(_)))
            );
        } else {
            assert!(text.contains("user answer") && text.contains("hook context"));
            assert_eq!(response.body(), &[Block::Text("user answer".into())]);
        }
        setup.owner.complete(message::Status::Done, vec![]).unwrap();
    }
}

#[tokio::test]
async fn an_internal_prompt_under_an_ask_invocation_does_not_answer_the_model_request() {
    let mut setup = Setup::new();
    let commands = Arc::new(Commands::new(setup.table.clone()));
    let tool = AskUser(commands.clone());
    let args = json!({"text":"Which?"});
    let call = llm::message::ToolCall {
        id: "native-ask".into(),
        name: "ask".into(),
        args: args.clone(),
    };
    let (kind, body) = tool.model_request(&args);
    let request = setup
        .input
        .tool_request(&call, kind, body)
        .unwrap()
        .unwrap();
    let ctx = setup.ctx().with_request(Some(request.clone()));
    let mut internal = commands
        .ask(
            (&ctx).into(),
            "hook",
            Asked {
                question: "Allow the capability?".into(),
                options: vec!["allow".into()],
                back: true,
            },
        )
        .unwrap()
        .unwrap();
    let question = setup.table.questions(setup.dir.path())[0].clone();
    assert!(question.request.is_none());
    setup.table.answer(question.id, Some("allow".into()));
    assert_eq!(internal.try_recv(), Ok(Some("allow".into())));
    assert!(
        setup
            .messages
            .response(&request.request_id().unwrap())
            .unwrap()
            .is_none()
    );
    let mut call = Box::pin(tool.execute(args, &ctx));
    assert!(futures::poll!(call.as_mut()).is_pending());
    let question = setup.table.questions(setup.dir.path())[0].clone();
    assert_eq!(question.request.as_ref(), Some(request.id()));
    setup
        .table
        .answer(question.id, Some("actual user answer".into()));
    assert_eq!(call.await.unwrap().flatten(), "actual user answer");
    let result = setup.input.completed_calls().unwrap().pop().unwrap();
    setup.input.tool_response(&result).unwrap();
    assert_eq!(
        setup
            .messages
            .response(&request.request_id().unwrap())
            .unwrap()
            .unwrap()
            .body(),
        &[Block::Text("actual user answer".into())]
    );
    setup.owner.complete(message::Status::Done, vec![]).unwrap();
}
