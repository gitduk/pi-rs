//! Shared semantic messages. Transport progress and runtime controls live elsewhere.

pub mod conversation;
mod tool;
pub use tool::{Text, ToolCall, ToolResult, ToolResultContent, flatten};

mod content;
pub use content::{Image, Reasoning, ReasoningContent};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_ID_BYTES: usize = 256;
/// Default encoded message budget shared by hosts and transports.
pub const DEFAULT_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: String) -> Result<Self, InvalidMessage> {
                if value.trim().is_empty() {
                    return Err(InvalidMessage::EmptyIdentifier);
                }
                if value.len() > MAX_ID_BYTES {
                    return Err(InvalidMessage::IdentifierLength);
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = InvalidMessage;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}
id!(MessageId);
id!(SessionId);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RequestId(MessageId);

impl RequestId {
    pub fn message(&self) -> &MessageId {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    User,
    Automatic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelRequest {
    Tool { name: String },
    Ask { options: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "direction",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    ToModel(InputKind),
    FromModel(ModelRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "failure",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Status {
    Done,
    Stopped,
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Block {
    Text(String),
    Image(Image),
    Data(Value),
    Reasoning(Reasoning),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Kind {
    Request(Request),
    Response { request: RequestId, status: Status },
}

/// Immutable after validation. Session ownership and pending state are checked by the host.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    #[serde(skip)]
    encoded_bytes: usize,
    id: MessageId,
    session: SessionId,
    by: Option<MessageId>,
    at: u64,
    kind: Kind,
    body: Vec<Block>,
}

impl Message {
    pub fn automatic_trigger(&self) -> Option<(&str, &str)> {
        if matches!(
            self.kind,
            Kind::Request(Request::ToModel(InputKind::Automatic))
        ) {
            trigger(&self.body)
        } else {
            None
        }
    }
    pub fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
    pub fn id(&self) -> &MessageId {
        &self.id
    }
    pub fn session(&self) -> &SessionId {
        &self.session
    }
    pub fn by(&self) -> Option<&MessageId> {
        self.by.as_ref()
    }
    pub fn at(&self) -> u64 {
        self.at
    }
    pub fn kind(&self) -> &Kind {
        &self.kind
    }
    pub fn body(&self) -> &[Block] {
        &self.body
    }
    pub fn request_id(&self) -> Option<RequestId> {
        match self.kind {
            Kind::Request(_) => Some(RequestId(self.id.clone())),
            Kind::Response { .. } => None,
        }
    }
}

/// Untrusted transport data; decoding does not admit or execute an interaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireMessage {
    pub id: MessageId,
    pub session: SessionId,
    pub by: Option<MessageId>,
    pub at: u64,
    pub kind: WireKind,
    pub body: Vec<Block>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WireKind {
    Request(Request),
    Response { request: MessageId, status: Status },
}

/// Admission limits, supplied by the host for every entry point.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub bytes: usize,
    pub blocks: usize,
    pub options: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidMessage {
    #[error("message identifiers must not be blank")]
    EmptyIdentifier,
    #[error("message identifier exceeds {} bytes", MAX_ID_BYTES)]
    IdentifierLength,
    #[error("message exceeds admission limits")]
    Limit,
    #[error("body does not match the request")]
    Body,
    #[error("response requires its original request in the same session")]
    Association,
    #[error("message cannot cause itself")]
    SelfCause,
    #[error("failure code and message must not be blank")]
    Failure,
}

impl WireMessage {
    pub fn automatic_trigger(&self) -> Option<(&str, &str)> {
        if matches!(
            self.kind,
            WireKind::Request(Request::ToModel(InputKind::Automatic))
        ) {
            trigger(&self.body)
        } else {
            None
        }
    }
    /// Check content and response correlation before the host admits a message.
    /// The host additionally checks source, causality, pending state and deduplication.
    pub fn validate(
        self,
        original: Option<&Message>,
        limits: Limits,
    ) -> Result<Message, InvalidMessage> {
        if self.by.as_ref() == Some(&self.id) {
            return Err(InvalidMessage::SelfCause);
        }
        if self.body.len() > limits.blocks {
            return Err(InvalidMessage::Limit);
        }
        // Stop serialization at the budget without allocating another payload.
        let mut budget = Budget(limits.bytes);
        serde_json::to_writer(&mut budget, &self).map_err(|_| InvalidMessage::Limit)?;
        let encoded_bytes = limits.bytes - budget.0;
        let kind = match self.kind {
            WireKind::Request(request) => {
                validate_request(&request, &self.body, limits)?;
                Kind::Request(request)
            }
            WireKind::Response { request, status } => {
                let original = original.ok_or(InvalidMessage::Association)?;
                let Kind::Request(ref target) = original.kind else {
                    return Err(InvalidMessage::Association);
                };
                if original.id != request || original.session != self.session || self.id == request
                {
                    return Err(InvalidMessage::Association);
                }
                validate_response(target, &status, &self.body)?;
                Kind::Response {
                    request: RequestId(request),
                    status,
                }
            }
        };
        Ok(Message {
            encoded_bytes,
            id: self.id,
            session: self.session,
            by: self.by,
            at: self.at,
            kind,
            body: self.body,
        })
    }
}

fn trigger(body: &[Block]) -> Option<(&str, &str)> {
    body.iter().find_map(|block| {
        let Block::Data(data) = block else {
            return None;
        };
        let source = data.get("source")?.as_str()?;
        let reason = data.get("reason")?.as_str()?;
        let event = data.get("event")?.as_str()?;
        (!source.trim().is_empty()
            && !reason.trim().is_empty()
            && !event.trim().is_empty()
            && source.len() <= MAX_ID_BYTES
            && event.len() <= MAX_ID_BYTES)
            .then_some((source, event))
    })
}

fn meaningful(body: &[Block]) -> bool {
    body.iter().any(|block| match block {
        Block::Text(text) => !text.trim().is_empty(),
        Block::Image(_) | Block::Data(_) => true,
        Block::Reasoning(_) => false,
    })
}

fn validate_request(
    request: &Request,
    body: &[Block],
    limits: Limits,
) -> Result<(), InvalidMessage> {
    let valid = match request {
        Request::ToModel(InputKind::User) => {
            meaningful(body) && !body.iter().any(|b| matches!(b, Block::Reasoning(_)))
        }
        Request::ToModel(InputKind::Automatic) => {
            !body.iter().any(|b| matches!(b, Block::Reasoning(_))) && trigger(body).is_some()
        }
        Request::FromModel(ModelRequest::Tool { name }) => {
            !name.trim().is_empty() && matches!(body, [Block::Data(_)])
        }
        Request::FromModel(ModelRequest::Ask { options }) => {
            if options.len() > limits.options {
                return Err(InvalidMessage::Limit);
            }
            options.iter().all(|s| !s.trim().is_empty())
                && options
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == options.len()
                && body
                    .iter()
                    .any(|b| matches!(b, Block::Text(s) if !s.trim().is_empty()))
                && body
                    .iter()
                    .all(|b| matches!(b, Block::Text(_) | Block::Image(_)))
        }
    };
    if valid {
        Ok(())
    } else {
        Err(InvalidMessage::Body)
    }
}

fn validate_response(
    request: &Request,
    status: &Status,
    body: &[Block],
) -> Result<(), InvalidMessage> {
    if let Status::Failed(failure) = status
        && (failure.code.trim().is_empty() || failure.message.trim().is_empty())
    {
        return Err(InvalidMessage::Failure);
    }
    let valid = match request {
        Request::ToModel(_) => true,
        Request::FromModel(ModelRequest::Tool { .. }) => {
            !body.iter().any(|b| matches!(b, Block::Reasoning(_)))
        }
        Request::FromModel(ModelRequest::Ask { .. }) => {
            body.iter()
                .all(|b| matches!(b, Block::Text(_) | Block::Image(_)))
                && (!matches!(status, Status::Done) || meaningful(body))
        }
    };
    if valid {
        Ok(())
    } else {
        Err(InvalidMessage::Body)
    }
}

struct Budget(usize);
impl std::io::Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("message too large"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        bytes: 4096,
        blocks: 16,
        options: 8,
    };

    fn wire(kind: WireKind) -> WireMessage {
        WireMessage {
            id: MessageId::new("m1".into()).unwrap(),
            session: SessionId::new("s1".into()).unwrap(),
            by: None,
            at: 1,
            kind,
            body: vec![Block::Text("hello".into())],
        }
    }

    #[test]
    fn responses_require_the_original_request_and_session() {
        let request = wire(WireKind::Request(Request::ToModel(InputKind::User)))
            .validate(None, LIMITS)
            .unwrap();
        let mut reply = wire(WireKind::Response {
            request: request.id().clone(),
            status: Status::Done,
        });
        reply.id = MessageId::new("m2".into()).unwrap();
        assert_eq!(
            reply.clone().validate(None, LIMITS),
            Err(InvalidMessage::Association)
        );
        reply.session = SessionId::new("s2".into()).unwrap();
        assert_eq!(
            reply.clone().validate(Some(&request), LIMITS),
            Err(InvalidMessage::Association)
        );
        reply.session = request.session().clone();
        let message = reply.validate(Some(&request), LIMITS).unwrap();
        let decoded: WireMessage =
            serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
        assert_eq!(decoded.validate(Some(&request), LIMITS).unwrap(), message);
    }

    #[test]
    fn empty_answer_is_invalid_but_stopping_needs_no_answer() {
        let request = wire(WireKind::Request(Request::FromModel(ModelRequest::Ask {
            options: vec![],
        })))
        .validate(None, LIMITS)
        .unwrap();
        let mut reply = wire(WireKind::Response {
            request: request.id().clone(),
            status: Status::Done,
        });
        reply.id = MessageId::new("m2".into()).unwrap();
        reply.body.clear();
        assert_eq!(
            reply.clone().validate(Some(&request), LIMITS),
            Err(InvalidMessage::Body)
        );
        reply.kind = WireKind::Response {
            request: request.id().clone(),
            status: Status::Stopped,
        };
        assert!(reply.validate(Some(&request), LIMITS).is_ok());
    }

    #[test]
    fn malformed_frames_and_oversized_metadata_are_rejected() {
        let mut frame =
            serde_json::to_value(wire(WireKind::Request(Request::ToModel(InputKind::User))))
                .unwrap();
        frame["id"] = Value::String(String::new());
        assert!(serde_json::from_value::<WireMessage>(frame).is_err());
        assert_eq!(
            MessageId::new("x".repeat(MAX_ID_BYTES + 1)),
            Err(InvalidMessage::IdentifierLength)
        );
        let huge = wire(WireKind::Request(Request::FromModel(ModelRequest::Ask {
            options: vec!["x".repeat(5000)],
        })));
        assert_eq!(huge.validate(None, LIMITS), Err(InvalidMessage::Limit));
    }
}
