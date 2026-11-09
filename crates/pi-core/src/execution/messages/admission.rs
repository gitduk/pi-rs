//! Checked input admission and ownership while an input waits for execution.

use super::{Error, Failure, LIMITS, Messages, Responder, new_id};
use message::{Block, Kind, Message, MessageId, Request, RequestId, Status, WireKind, WireMessage};
use std::borrow::Cow;
use std::sync::Arc;

impl Messages {
    pub fn admit(
        &self,
        origin: crate::driver::Origin,
        prompt: agent::session::Prompt,
    ) -> Result<AdmittedInput, Error> {
        let wire = self.input_wire(origin, &prompt)?;
        let message = wire.validate(None, LIMITS)?;
        match self.admit_message(message, prompt)? {
            Admission::Accepted(input) => Ok(input),
            Admission::Existing { .. } => Err(Error::Conflict),
        }
    }

    /// Semantic input bypasses text command parsing; its session and kind are explicit.
    pub fn admit_wire(&self, wire: WireMessage) -> Result<Admission, Error> {
        let message = wire.validate(None, LIMITS)?;
        let Kind::Request(Request::ToModel(kind)) = message.kind() else {
            return Err(Error::Parent);
        };
        let mut text = Vec::new();
        let mut images = Vec::new();
        for block in message.body() {
            match block {
                Block::Text(value) => text.push(Cow::Borrowed(value.as_str())),
                Block::Data(value) => text.push(Cow::Owned(value.to_string())),
                Block::Image(image) => images.push(image.clone()),
                Block::Reasoning(_) => return Err(Error::Parent),
            }
        }
        let automatic = *kind == message::InputKind::Automatic;
        let prompt = agent::session::Prompt {
            text: text.join("\n"),
            images,
            shown: automatic.then(|| "automatic input".into()),
            relayed: automatic.then(|| "This input was submitted by a program.".into()),
        };
        self.admit_message(message, prompt)
    }

    fn admit_message(
        &self,
        message: Message,
        prompt: agent::session::Prompt,
    ) -> Result<Admission, Error> {
        let mut table = self.lock();
        let (id, fresh) = table.insert_input(message)?;
        if !fresh {
            return Ok(Admission::Existing {
                response: table.requests[id.message()].response.clone(),
                request: id,
            });
        }
        let original = table.requests[id.message()].request.clone();
        Ok(Admission::Accepted(AdmittedInput {
            prompt,
            ticket: PendingInput {
                messages: self.clone(),
                original,
                started: false,
            },
        }))
    }

    fn start(&self, id: &MessageId) -> Result<Responder, Error> {
        let mut table = self.lock();
        if table.active.is_some() || table.waiting.front() != Some(id) {
            return Err(Error::Busy);
        }
        table.waiting.pop_front();
        table.queued_bytes -= table.requests[id].request.encoded_bytes();
        table.active = Some(id.clone());
        Ok(Responder {
            messages: self.clone(),
            original: table.requests[id].request.clone(),
            finished: false,
        })
    }

    fn stop_waiting(&self, id: &MessageId, status: Status) {
        let path = {
            let mut table = self.lock();
            if table.active.as_ref() == Some(id)
                || table
                    .requests
                    .get(id)
                    .is_none_or(|record| record.response.is_some())
            {
                return;
            }
            if table.waiting.contains(id) {
                table.queued_bytes -= table.requests[id].request.encoded_bytes();
            }
            let wire = WireMessage {
                id: new_id(),
                session: table.session.clone(),
                by: Some(id.clone()),
                at: agent::session::now(),
                kind: WireKind::Response {
                    request: id.clone(),
                    status,
                },
                body: Vec::new(),
            };
            if let Err(error) = table.finish(id, wire) {
                table.waiting.retain(|queued| queued != id);
                if let Some(record) = table.requests.get_mut(id) {
                    record.abandoned = true;
                }
                tracing::warn!(target: "pi::session", request = id.as_str(), error = %error, "queued input could not be closed");
                return;
            }
            table.waiting.retain(|queued| queued != id);
            table.cancellation_epoch += 1;
            if table.cancellation_save {
                return;
            }
            let path = table
                .archive
                .as_ref()
                .map(|archive| archive.directory().to_path_buf());
            if path.is_some() {
                table.cancellation_save = true;
            }
            path
        };
        if let Some(path) = path {
            let messages = self.clone();
            crate::core::saves::behind(move || {
                loop {
                    let epoch = messages.lock().cancellation_epoch;
                    let result = messages.snapshot().save(&path);
                    let mut table = messages.lock();
                    if let Err(error) = result {
                        table.cancellation_save = false;
                        tracing::warn!(target: "pi::session", session = table.session.as_str(), error = %error, "cancelled input was not saved");
                        break;
                    }
                    if epoch == table.cancellation_epoch {
                        table.cancellation_save = false;
                        break;
                    }
                }
            });
        }
    }

    fn input_wire(
        &self,
        origin: crate::driver::Origin,
        prompt: &agent::session::Prompt,
    ) -> Result<WireMessage, Error> {
        let id = new_id();
        let mut body = vec![Block::Text(prompt.text.clone())];
        body.extend(prompt.images.iter().cloned().map(Block::Image));
        let kind = match origin {
            crate::driver::Origin::Message(_) => return Err(Error::Parent),
            crate::driver::Origin::Typed | crate::driver::Origin::Input(_) => {
                message::InputKind::User
            }
            crate::driver::Origin::Background => {
                body.push(Block::Data(serde_json::json!({ "source": "background", "reason": prompt.relayed.as_deref().unwrap_or("background result"), "event": id.as_str() })));
                message::InputKind::Automatic
            }
        };
        let wire = WireMessage {
            id,
            session: self.session(),
            by: None,
            at: agent::session::now(),
            kind: WireKind::Request(Request::ToModel(kind)),
            body,
        };
        Ok(wire)
    }
}

/// Retries report the original request without granting a second execution ticket.
pub enum Admission {
    Accepted(AdmittedInput),
    Existing {
        request: RequestId,
        response: Option<Arc<Message>>,
    },
}

/// An admitted immutable model input, owned by its queue until execution starts.
pub struct AdmittedInput {
    prompt: agent::session::Prompt,
    ticket: PendingInput,
}

impl AdmittedInput {
    pub fn message(&self) -> &Message {
        &self.ticket.original
    }
    pub(crate) fn presentation(mut self, shown: String, relayed: String) -> Self {
        self.prompt.shown = Some(shown);
        self.prompt.relayed = Some(relayed);
        self
    }
    pub fn prompt(&self) -> &agent::session::Prompt {
        &self.prompt
    }
    pub fn is_next(&self) -> bool {
        let table = self.ticket.messages.lock();
        table.active.is_none() && table.waiting.front() == Some(self.ticket.original.id())
    }

    pub fn start(self) -> Result<(agent::session::Prompt, Responder), Error> {
        let Self { prompt, mut ticket } = self;
        let responder = ticket.messages.start(ticket.original.id())?;
        ticket.started = true;
        Ok((prompt, responder))
    }

    pub fn reject(mut self, failure: Failure) {
        self.ticket
            .messages
            .stop_waiting(self.ticket.original.id(), Status::Failed(failure));
        self.ticket.started = true;
    }
}

struct PendingInput {
    messages: Messages,
    original: Arc<Message>,
    started: bool,
}

impl Drop for PendingInput {
    fn drop(&mut self) {
        if !self.started {
            self.messages
                .stop_waiting(self.original.id(), Status::Stopped);
        }
    }
}
