//! The host owns one response port for each model-directed user input.

use super::{Inner, Table};
use crate::execution::messages::{Messages, Responder, new_id};
use message::{
    Block, Kind, Message, MessageId, ModelRequest, Request, Status, WireKind, WireMessage,
};
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct UserInput {
    pub(super) root: PathBuf,
    pub(super) port: Responder,
}

pub(super) fn text(response: &Message) -> Option<String> {
    let Kind::Response { status, .. } = response.kind() else {
        return None;
    };
    match status {
        Status::Stopped => return None,
        Status::Failed(failure) => return Some(failure.message.clone()),
        Status::Done => {}
    }
    Some(
        response
            .body()
            .iter()
            .map(|block| match block {
                Block::Text(text) => text.clone(),
                Block::Image(_) => "[image]".into(),
                Block::Data(_) | Block::Reasoning(_) => unreachable!("validated user response"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

impl Table {
    pub(crate) fn user_request(
        &self,
        root: PathBuf,
        messages: &Messages,
        parent: &message::RequestId,
        wire: WireMessage,
        native_call: String,
    ) -> Result<Arc<Message>, String> {
        if !matches!(
            &wire.kind,
            WireKind::Request(Request::FromModel(ModelRequest::Ask { .. }))
        ) {
            return Err("response port does not belong to user input".into());
        }
        // Publication can wake an observer here; its response waits on this
        // same lock until the result port has been installed.
        let mut inner = self.lock();
        let root = inner
            .inputs
            .get(&wire.session)
            .and_then(std::sync::Weak::upgrade)
            .map_or(root, |ingress| ingress.root.clone());
        let port = messages
            .child(parent, wire, Some(native_call))
            .map_err(|e| e.to_string())?;
        let request = port.request();
        tracing::info!(target: "pi::background", request = request.id().as_str(), session = request.session().as_str(), "user input registered");
        inner
            .user_inputs
            .insert(request.id().clone(), UserInput { root, port });
        Ok(request)
    }

    pub(crate) fn user_response(&self, id: &MessageId) -> Result<Option<Arc<Message>>, String> {
        let inner = self.lock();
        let input = inner
            .user_inputs
            .get(id)
            .ok_or("user input is no longer registered")?;
        input.port.response().map_err(|e| e.to_string())
    }

    /// Close unanswered input before native cleanup; an accepted answer stays final.
    pub(crate) fn seal_user_input(
        &self,
        id: &MessageId,
        status: Status,
        body: Vec<Block>,
    ) -> Result<Arc<Message>, String> {
        let mut inner = self.lock();
        let input = inner
            .user_inputs
            .get_mut(id)
            .ok_or("user input is no longer registered")?;
        let response = if input.port.is_finished() {
            input
                .port
                .response()
                .map_err(|e| e.to_string())?
                .ok_or("user input lost its response")?
        } else {
            input
                .port
                .complete(status, body)
                .map_err(|e| e.to_string())?
        };
        inner.deliver_user_response(id, &response);
        drop(inner);
        self.wake();
        Ok(response)
    }

    pub(crate) fn retire_user_input(&self, id: &MessageId) -> Result<(), String> {
        let mut inner = self.lock();
        if inner
            .user_inputs
            .get(id)
            .is_some_and(|input| !input.port.is_finished())
        {
            return Err("user input has no terminal response".into());
        }
        inner.user_inputs.remove(id);
        Ok(())
    }

    pub(crate) fn abandon_user_input(&self, id: &MessageId) {
        let input = {
            let mut inner = self.lock();
            let question = inner
                .standing
                .iter()
                .find(|(_, q)| q.request.as_ref() == Some(id))
                .map(|(_, q)| q.id);
            if let Some(question) = question {
                inner.close(question, "its response owner exited");
                if let Some(to) = inner.answers.remove(&question) {
                    let _ = to.send(None);
                }
            }
            inner.user_inputs.remove(id)
        };
        if input.is_some() {
            tracing::info!(target: "pi::background", request = id.as_str(), "user input owner exited");
            drop(input);
            self.wake();
        }
    }

    pub(super) fn accept_user_response(
        &self,
        messages: &Messages,
        wire: WireMessage,
    ) -> Result<pi_tools::runtime::channel::Receipt, String> {
        let WireKind::Response { request: id, .. } = &wire.kind else {
            return Err("expected a user response".into());
        };
        let id = id.clone();
        let (request, response) = {
            let mut inner = self.lock();
            if let Some(input) = inner.user_inputs.get_mut(&id) {
                let request = input.port.request().request_id().expect("stored request");
                let response = input.port.finish(wire).map_err(|e| e.to_string())?;
                inner.deliver_user_response(&id, &response);
                (request, response)
            } else {
                let original = messages
                    .request(&id)
                    .map_err(|e| e.to_string())?
                    .ok_or("unknown user input request")?;
                if !matches!(
                    original.kind(),
                    Kind::Request(Request::FromModel(ModelRequest::Ask { .. }))
                ) {
                    return Err("connection cannot answer this request".into());
                }
                let request = original.request_id().expect("stored request");
                let stored = messages
                    .response(&request)
                    .map_err(|e| e.to_string())?
                    .ok_or("user input has no response port")?;
                let candidate = wire
                    .validate(Some(&original), crate::execution::messages::LIMITS)
                    .map_err(|e| e.to_string())?;
                if stored.as_ref() != &candidate {
                    return Err("user input already completed with another response".into());
                }
                (request, stored)
            }
        };
        self.wake();
        Ok(pi_tools::runtime::channel::Receipt {
            cursor: messages
                .request_cursor(&request)
                .map_err(|e| e.to_string())?,
            request,
            response: Some(response),
        })
    }
}

impl Inner {
    pub(super) fn model_input(&self, session: &str, id: &MessageId) -> Option<&UserInput> {
        self.user_inputs
            .get(id)
            .filter(|input| input.port.request().session().as_str() == session)
    }

    pub(super) fn answer_text(
        &mut self,
        id: &MessageId,
        picked: Option<String>,
    ) -> Result<(), String> {
        let input = self
            .user_inputs
            .get_mut(id)
            .ok_or("user input is no longer registered")?;
        let request = input.port.request();
        let (status, body) = match picked {
            Some(text) => (Status::Done, vec![Block::Text(text)]),
            None => (Status::Stopped, vec![]),
        };
        let response = input
            .port
            .finish(WireMessage {
                id: new_id(),
                session: request.session().clone(),
                by: Some(id.clone()),
                at: agent::session::now(),
                kind: WireKind::Response {
                    request: id.clone(),
                    status,
                },
                body,
            })
            .map_err(|e| e.to_string())?;
        self.deliver_user_response(id, &response);
        Ok(())
    }

    fn deliver_user_response(&mut self, id: &MessageId, response: &Message) {
        let question = self
            .standing
            .iter()
            .find(|(_, q)| q.request.as_ref() == Some(id))
            .map(|(_, q)| q.id);
        if let Some(question) = question {
            self.close(question, "user input completed");
            if let Some(to) = self.answers.remove(&question) {
                let _ = to.send(text(response));
            }
        }
    }
}
