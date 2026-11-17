//! What reaches a running conversation at its next request boundary.

use std::path::PathBuf;
use std::sync::Arc;

use ::execution::channel::{Outcome, Response};
use agent::seams::Note;
use agent::session::Prompt;

use super::background::Table;
use crate::core::lane::Notes;
use crate::execution::queue::{Pending, Queue, Queued};

/// What a running turn takes at its request boundaries: notes, and the lane's
/// queued inputs, joined rather than started.
pub struct Input {
    root: PathBuf,
    executions: Arc<Table>,
    notes: Option<Notes>,
    queue: Option<Queue>,
    messages: Option<Calls>,
    joined: std::sync::Mutex<Vec<Joined>>,
    checkpoint: Option<(crate::core::lane::Save, Arc<llm::meter::Meter>)>,
}

/// An input this run took, with the operation waiting to hear how it went.
struct Joined {
    responder: crate::execution::messages::Responder,
    operation: crate::execution::Request,
}

struct Calls {
    messages: crate::execution::messages::Messages,
    parent: message::RequestId,
    pending: std::sync::Mutex<std::collections::HashMap<String, Call>>,
}

struct Call {
    name: String,
    reply: Reply,
}

enum Reply {
    Tool(crate::execution::messages::Responder),
    User(message::RequestId),
}

/// Project a validated user answer into the native tool result representation.
pub(crate) fn answer_content(body: &[message::Block]) -> Vec<llm::message::ToolResultContent> {
    body.iter()
        .map(|block| match block {
            message::Block::Text(text) => {
                llm::message::ToolResultContent::Text(llm::message::Text { text: text.clone() })
            }
            message::Block::Image(image) => llm::message::ToolResultContent::Image(image.clone()),
            message::Block::Data(_) | message::Block::Reasoning(_) => {
                unreachable!("validated user response")
            }
        })
        .collect()
}

/// How the run ended, for the operation whose input it took.
fn outcome(status: &message::Status) -> Outcome {
    match status {
        message::Status::Done => Outcome::Done,
        message::Status::Stopped => Outcome::Stopped(String::new()),
        message::Status::Failed(failure) => Outcome::Failed(failure.message.clone()),
    }
}

impl Input {
    pub fn new(root: PathBuf, executions: Arc<Table>, notes: Option<Notes>) -> Self {
        Self {
            root,
            executions,
            notes,
            queue: None,
            messages: None,
            joined: Default::default(),
            checkpoint: None,
        }
    }

    pub(crate) fn with_checkpoint(
        mut self,
        save: crate::core::lane::Save,
        meter: Arc<llm::meter::Meter>,
    ) -> Self {
        self.checkpoint = Some((save, meter));
        self
    }

    /// The lane's queue: what the frontend submitted while this run holds it.
    pub(crate) fn with_queue(mut self, queue: Queue) -> Self {
        self.queue = Some(queue);
        self
    }

    /// Answer every input this run took with its terminal outcome.
    pub(crate) fn close_results(
        &self,
        status: message::Status,
        body: &[message::Block],
    ) -> Result<(), String> {
        let mut joined = self
            .joined
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if joined.is_empty() {
            return Ok(());
        }
        let event = Response::Finished {
            text: super::text_of(body),
            outcome: outcome(&status),
        };
        for mut taken in joined.drain(..) {
            super::tell_operation(&self.executions, taken.operation, event.clone());
            taken
                .responder
                .complete(status.clone(), body.to_vec())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn with_messages(
        mut self,
        messages: crate::execution::messages::Messages,
        parent: message::RequestId,
    ) -> Self {
        self.abandon_calls();
        self.messages = Some(Calls {
            messages,
            parent,
            pending: Default::default(),
        });
        self
    }

    pub fn close_calls(&self, status: message::Status) -> Result<(), String> {
        if let Some(calls) = &self.messages {
            let mut pending = calls
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for call in pending.values_mut() {
                match &mut call.reply {
                    Reply::Tool(port) => {
                        port.complete(status.clone(), Vec::new())
                            .map_err(|e| e.to_string())?;
                    }
                    Reply::User(request) => {
                        self.executions.seal_user_input(
                            request.message(),
                            status.clone(),
                            vec![],
                        )?;
                        self.executions.retire_user_input(request.message())?;
                    }
                }
            }
            pending.clear();
        }
        Ok(())
    }
}

impl Input {
    fn user_results(
        &self,
        closing: Option<message::Status>,
    ) -> Result<Vec<llm::message::ToolResult>, String> {
        let Some(calls) = &self.messages else {
            return Ok(vec![]);
        };
        let pending = calls
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut results = Vec::new();
        for (native, call) in pending.iter() {
            let Reply::User(request) = &call.reply else {
                continue;
            };
            let response = match &closing {
                Some(status) => Some(self.executions.seal_user_input(
                    request.message(),
                    status.clone(),
                    vec![],
                )?),
                None => self.executions.user_response(request.message())?,
            };
            let Some(response) = response else { continue };
            let mut content = answer_content(response.body());
            let message::Kind::Response { status, .. } = response.kind() else {
                unreachable!("stored response")
            };
            let notice = match status {
                message::Status::Done => None,
                message::Status::Stopped => Some(agent::session::STOPPED_CALL.to_owned()),
                message::Status::Failed(failure) => {
                    Some(format!("{} [code: {}]", failure.message, failure.code))
                }
            };
            if let Some(text) = notice {
                content.push(llm::message::ToolResultContent::Text(llm::message::Text {
                    text,
                }));
            }
            results.push(llm::message::ToolResult {
                call: native.clone(),
                name: call.name.clone(),
                content,
                is_error: !matches!(status, message::Status::Done),
            });
        }
        Ok(results)
    }
}

impl Input {
    fn abandon_calls(&mut self) {
        if let Some(calls) = &mut self.messages {
            let pending = calls
                .pending
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for call in pending.values() {
                if let Reply::User(request) = &call.reply {
                    self.executions.abandon_user_input(request.message());
                }
            }
            pending.clear();
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.abandon_calls();
    }
}

impl agent::seams::Input for Input {
    fn checkpoint(
        &self,
        session: &agent::session::Session,
    ) -> futures::future::BoxFuture<'_, Result<(), String>> {
        let snapshot =
            self.checkpoint
                .as_ref()
                .zip(self.messages.as_ref())
                .map(|((save, meter), calls)| {
                    (
                        save.clone(),
                        meter.clone(),
                        calls.messages.snapshot(),
                        session.clone(),
                    )
                });
        Box::pin(async move {
            if let Some((save, meter, messages, session)) = snapshot {
                crate::core::saves::wait(move || save.write(&session, &messages, meter.read()))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("conversation checkpoint failed: {e:#}"))?;
            }
            Ok(())
        })
    }

    fn take(&self) -> Option<Prompt> {
        let calls = self.messages.as_ref()?;
        let queue = self.queue.as_ref()?;
        loop {
            let Queued {
                input,
                request,
                note,
                ..
            } = queue.pop_joining(&calls.parent)?;
            let Pending::Prompt(admitted) = input else {
                unreachable!("a host operation forms a boundary")
            };
            let linked = admitted.message().id().clone();
            // Refused: the input left the queue elsewhere, answered or stopped.
            let Ok((prompt, responder)) = admitted.join(&calls.parent) else {
                continue;
            };
            super::tell_operation(&self.executions, request, Response::Started);
            super::link_operation(&self.executions, request, linked.clone());
            if !note.is_empty()
                && let Some(notes) = &self.notes
            {
                notes.hold(note);
            }
            self.joined
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Joined {
                    responder,
                    operation: request,
                });
            tracing::info!(target: "pi::session", lane = request.lane, request = request.id, message_id = linked.as_str(), "input joined the running turn");
            return Some(prompt);
        }
    }

    fn tool_request(
        &self,
        call: &llm::message::ToolCall,
        kind: message::ModelRequest,
        body: Vec<message::Block>,
    ) -> Result<Option<Arc<message::Message>>, String> {
        let Some(calls) = &self.messages else {
            return Ok(None);
        };
        let mut pending = calls
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.contains_key(&call.id) {
            return Err("duplicate active native tool call identifier".into());
        }
        let user = matches!(&kind, message::ModelRequest::Ask { .. });
        let wire = message::WireMessage {
            id: crate::execution::messages::new_id(),
            session: calls.messages.session(),
            by: Some(calls.parent.message().clone()),
            at: agent::session::now(),
            kind: message::WireKind::Request(message::Request::FromModel(kind)),
            body,
        };
        let (request, reply) = if user {
            let request = self.executions.user_request(
                self.root.clone(),
                &calls.messages,
                &calls.parent,
                wire,
                call.id.clone(),
            )?;
            let reply = Reply::User(request.request_id().expect("model user input"));
            (request, reply)
        } else {
            let port = calls
                .messages
                .child(&calls.parent, wire, Some(call.id.clone()))
                .map_err(|e| e.to_string())?;
            (port.request(), Reply::Tool(port))
        };
        pending.insert(
            call.id.clone(),
            Call {
                name: call.name.clone(),
                reply,
            },
        );
        Ok(Some(request))
    }

    fn tool_response(&self, result: &llm::message::ToolResult) -> Result<(), String> {
        let Some(calls) = &self.messages else {
            return Ok(());
        };
        let mut pending = calls
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let call = pending
            .get_mut(&result.call)
            .ok_or("result has no active tool request")?;
        if call.name != result.name {
            return Err("result tool name does not match its request".into());
        }
        let body = result
            .content
            .iter()
            .map(|block| match block {
                llm::message::ToolResultContent::Text(text) => {
                    message::Block::Text(text.text.clone())
                }
                llm::message::ToolResultContent::Image(image) => {
                    message::Block::Image(image.clone())
                }
                llm::message::ToolResultContent::Json { value } => {
                    message::Block::Data(value.clone())
                }
            })
            .collect();
        let status = if result.is_error {
            message::Status::Failed(message::Failure {
                code: "TOOL_FAILED".into(),
                message: "tool could not complete the request".into(),
            })
        } else {
            message::Status::Done
        };
        match &mut call.reply {
            Reply::Tool(port) => {
                port.complete(status, body).map_err(|e| e.to_string())?;
            }
            Reply::User(request) => {
                self.executions
                    .seal_user_input(request.message(), status, body)?;
                self.executions.retire_user_input(request.message())?;
            }
        }
        pending.remove(&result.call);
        Ok(())
    }

    fn completed_calls(&self) -> Result<Vec<llm::message::ToolResult>, String> {
        self.user_results(None)
    }

    fn settle_calls(
        &self,
        status: message::Status,
    ) -> Result<Vec<llm::message::ToolResult>, String> {
        self.user_results(Some(status))
    }

    fn note(&self) -> Option<Note> {
        if let Some(text) = self.notes.as_ref().and_then(Notes::take) {
            return Some(Note { text, shown: true });
        }
        let session = self.messages.as_ref().map(|calls| calls.messages.session());
        let text = self
            .executions
            .untold_for(&self.root, session.as_ref().map(message::SessionId::as_str))?;
        Some(Note { text, shown: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::execution::background::RunSink as _;
    use agent::seams::Input as _;

    fn prompt(text: &str) -> Prompt {
        Prompt {
            text: text.into(),
            images: vec![],
            shown: None,
            relayed: None,
            context: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_turn_without_a_conversation_leaves_results_to_the_host() {
        let root = PathBuf::from("/checkout");
        let other = PathBuf::from("/elsewhere");
        let executions = Arc::new(Table::default());
        let sink = crate::driver::background::Commands::new(executions.clone());
        let phone = sink.start(
            root.clone().into(),
            ::execution::background::Invocation::shell("phone"),
            Default::default(),
        );
        phone
            .channel()
            .input("a remote question".into(), String::new());
        phone.channel().result("phone result".into());
        let first = sink.start(
            root.clone().into(),
            ::execution::background::Invocation::shell("build"),
            Default::default(),
        );
        let elsewhere = sink.start(
            other.clone().into(),
            ::execution::background::Invocation::shell("other"),
            Default::default(),
        );
        let second = sink.start(
            root.clone().into(),
            ::execution::background::Invocation::shell("test"),
            Default::default(),
        );
        first.channel().result("built".into());
        first.end();
        second.channel().result("tested".into());
        second.end();
        elsewhere.channel().result("unrelated".into());
        elsewhere.end();

        let input = Input::new(root.clone(), executions.clone(), None);
        assert!(input.take().is_none());
        let results = executions.take_ready_results(&root).unwrap();
        assert!(results.line.contains("phone result"));
        assert!(results.line.contains("built") && results.line.contains("tested"));
        assert!(!results.line.contains("unrelated"));
        let (_, super::super::background::Back::Input { text, .. }) =
            executions.take_back(&root).unwrap()
        else {
            panic!("the remote input still opens its own turn");
        };
        assert_eq!(text, "a remote question");
        assert!(executions.take_back(&root).is_none());
        assert!(executions.take_back(&other).is_some());
        phone.channel().result("later".into());
        assert!(input.take().is_none());
        assert!(
            executions
                .take_ready_results(&root)
                .unwrap()
                .line
                .contains("later")
        );
        assert!(input.take().is_none());
    }

    #[tokio::test]
    async fn a_joined_input_closes_the_operation_that_submitted_it() {
        use crate::execution::messages::Messages;
        use ::execution::channel::Told;
        let root = PathBuf::from("/checkout");
        let executions = Arc::new(Table::default());
        let (tx, mut rx) = ::execution::channel::channel();
        let operation = crate::driver::background::Commands::new(executions.clone()).start(
            root.clone().into(),
            ::execution::background::Invocation::shell("wechat"),
            Default::default(),
        );
        operation.channel().listen(tx);
        let messages = Messages::new(message::SessionId::new("session".into()).unwrap());
        let (_, mut owner) = messages
            .admit(crate::driver::Origin::Typed, prompt("run"))
            .unwrap()
            .start()
            .unwrap();
        let queued = messages
            .admit(
                crate::driver::Origin::Input(operation.id()),
                prompt("steer"),
            )
            .unwrap();
        let queued_id = queued.message().id().clone();
        let queued_request = queued.message().request_id().unwrap();
        let request =
            crate::execution::Request::new(crate::driver::Origin::Input(operation.id()), 1);
        let queue = Queue::default();
        queue.push(Queued {
            input: Pending::Prompt(queued),
            request,
            line: None,
            note: String::new(),
        });
        let input = Input::new(root, executions.clone(), None)
            .with_queue(queue)
            .with_messages(messages.clone(), owner.request().request_id().unwrap());

        assert_eq!(input.take().expect("the queued input joins").text, "steer");
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Started,
            })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Linked {
                operation: request.id,
                request: queued_id,
            })
        );
        assert!(messages.response(&queued_request).unwrap().is_none());

        input
            .close_results(
                message::Status::Done,
                &[message::Block::Text("answer".into())],
            )
            .unwrap();
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Response {
                request: request.id,
                event: Response::Finished {
                    text: "answer".into(),
                    outcome: Outcome::Done,
                },
            })
        );
        assert!(messages.response(&queued_request).unwrap().is_some());
        owner.complete(message::Status::Done, vec![]).unwrap();
    }
    #[test]
    fn tool_result_closes_its_parent_dependency_once() {
        use crate::execution::messages::Messages;
        use agent::seams::Input as _;
        let messages = Messages::new(message::SessionId::new("session".into()).unwrap());
        let (_, mut owner) = messages
            .admit(crate::driver::Origin::Typed, prompt("run"))
            .unwrap()
            .start()
            .unwrap();
        let input = Input::new(PathBuf::from("/checkout"), Arc::default(), None)
            .with_messages(messages, owner.request().request_id().unwrap());
        let call = llm::message::ToolCall {
            id: "native-1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "false"}),
        };
        input
            .tool_request(
                &call,
                message::ModelRequest::Tool {
                    name: call.name.clone(),
                },
                vec![message::Block::Data(call.args.clone())],
            )
            .unwrap();
        assert!(matches!(
            owner.complete(message::Status::Done, vec![]),
            Err(crate::execution::messages::Error::Children)
        ));
        let result = llm::message::ToolResult {
            call: call.id,
            name: call.name,
            content: vec![llm::message::ToolResultContent::Json {
                value: serde_json::json!({"exit_code": 1}),
            }],
            is_error: false,
        };
        input.tool_response(&result).unwrap();
        assert!(input.tool_response(&result).is_err());
        owner.complete(message::Status::Done, vec![]).unwrap();
    }
}
