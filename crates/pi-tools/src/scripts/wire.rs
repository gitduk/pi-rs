//! The sole fd 3 codec: one request or notice per line, one reply per request.
//!
//! A line carrying `id` is a request and is answered exactly once; a line
//! without it is a notice and is never answered. Both directions use the same
//! two shapes, which is what keeps one dispatch table enough.

use crate::runtime::channel::Cursor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

pub const MAX_FRAME_BYTES: usize = message::DEFAULT_MESSAGE_BYTES + 4096;
const MAX_LINE: usize = MAX_FRAME_BYTES + 1;
pub const SESSION_VAR: &str = "PI_SESSION_ID";
pub const REQUEST_VAR: &str = "PI_REQUEST_ID";
/// The protocol revision this build speaks; a mismatch is refused, not guessed.
pub const PROTOCOL: u32 = 5;

/// A request's name for itself: whatever the sender chose, echoed unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(u64),
    Text(String),
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::Text(text) => write!(f, "{text}"),
        }
    }
}

/// What a `listen` subscribes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    #[default]
    Session,
    Questions,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Incoming {
    Initialize {
        id: Id,
        protocol: u32,
        #[serde(default)]
        client: String,
    },
    Info {
        id: Id,
    },
    Send {
        #[serde(default)]
        id: Option<Id>,
        text: String,
        #[serde(default)]
        note: String,
        #[serde(default)]
        more: bool,
    },
    Event {
        #[serde(default)]
        id: Option<Id>,
        text: String,
        source: String,
        reason: String,
        #[serde(default)]
        key: Option<String>,
    },
    Reply {
        #[serde(default)]
        id: Option<Id>,
        to: message::MessageId,
        #[serde(default)]
        text: String,
        #[serde(default)]
        cancel: bool,
    },
    Ask {
        #[serde(default)]
        id: Option<Id>,
        text: String,
        #[serde(default)]
        options: Vec<String>,
    },
    Listen {
        id: Id,
        #[serde(default)]
        to: Stream,
        #[serde(default)]
        cursor: Option<Cursor>,
    },
    Status {
        #[serde(default)]
        text: Option<String>,
    },
    Stop {},
}

impl Incoming {
    /// What only the frame's own contents can refuse, before it reaches pi.
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        match self {
            Self::Send { text, .. } if text.trim().is_empty() => {
                return Err(("frame", "send needs text".into()));
            }
            Self::Event {
                text,
                source,
                reason,
                ..
            } => {
                if text.trim().is_empty() {
                    return Err(("frame", "an event needs text".into()));
                }
                if source.trim().is_empty() || reason.trim().is_empty() {
                    return Err(("frame", "an event needs source and reason".into()));
                }
            }
            Self::Reply {
                text,
                cancel: false,
                ..
            } if text.trim().is_empty() => {
                return Err(("frame", "a reply needs text or cancel".into()));
            }
            Self::Ask { text, .. } if text.trim().is_empty() => {
                return Err(("frame", "a question needs text".into()));
            }
            _ => {}
        }
        Ok(())
    }

    /// The request this line answers to, when it is one.
    pub fn id(&self) -> Option<&Id> {
        match self {
            Self::Initialize { id, .. } | Self::Info { id } | Self::Listen { id, .. } => Some(id),
            Self::Send { id, .. }
            | Self::Event { id, .. }
            | Self::Reply { id, .. }
            | Self::Ask { id, .. } => id.as_ref(),
            Self::Status { .. } | Self::Stop {} => None,
        }
    }
}

pub fn encode(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("frame exceeds the byte limit".into());
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// The answer to one request.
pub fn reply(id: &Id, result: Value) -> Value {
    json!({"id": id, "result": result})
}

/// Why one request could not be done.
pub fn failure(id: &Id, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "error": {"code": code, "message": message.into()}})
}

/// A line that could not even be read: there is no id to answer.
pub fn unreadable(code: &str, message: impl Into<String>) -> Value {
    json!({"error": {"code": code, "message": message.into()}})
}

/// Answer a frame with an error: bound to `id` when present, or unreadable without one.
pub fn error(id: Option<&Id>, code: &str, message: impl Into<String>) -> Value {
    match id {
        Some(id) => failure(id, code, message),
        None => unreadable(code, message),
    }
}

/// Decode and validate one incoming frame, returning the wire error frame on failure.
pub fn decode(value: &Value) -> Result<Incoming, Value> {
    let incoming = Incoming::deserialize(value).map_err(|err| {
        let id = value.get("id").and_then(|id| Id::deserialize(id).ok());
        error(id.as_ref(), "frame", format!("invalid frame: {err}"))
    })?;
    if let Err((code, why)) = incoming.validate() {
        return Err(error(incoming.id(), code, why));
    }
    Ok(incoming)
}

/// Reply to `initialize`: agree on protocol and advertise capabilities, or refuse a mismatch.
pub fn initialize(
    id: &Id,
    protocol: u32,
    mode: &str,
    session: Option<&str>,
    workspace: &Path,
    capabilities: &[&str],
) -> Value {
    if protocol == PROTOCOL {
        reply(
            id,
            json!({
                "protocol": PROTOCOL,
                "mode": mode,
                "session": session,
                "workspace": workspace,
                "capabilities": capabilities,
            }),
        )
    } else {
        failure(
            id,
            "protocol",
            format!("this pi speaks {PROTOCOL}; the client asked for {protocol}"),
        )
    }
}

/// Reply to `info`: current session, workspace, and whether a turn is active or queued.
pub fn info(id: &Id, session: Option<&str>, workspace: &Path, busy: bool) -> Value {
    reply(
        id,
        json!({
            "session": session,
            "workspace": workspace,
            "busy": busy,
        }),
    )
}

/// Reply to an accepted `send`, `event`, or `reply` request with its message id.
pub fn accepted(id: &Id, message: &str) -> Value {
    reply(
        id,
        json!({
            "accepted": true,
            "message": message,
        }),
    )
}

/// Line-delimited JSON frame reader, bounded to [`MAX_FRAME_BYTES`].
pub struct Frames<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
    skipping: bool,
    open: bool,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            line: Vec::new(),
            skipping: false,
            open: true,
        }
    }

    pub async fn next(&mut self) -> Result<Value, String> {
        while self.open {
            let room = (MAX_LINE + 1 - self.line.len()) as u64;
            let read = (&mut self.reader)
                .take(room)
                .read_until(b'\n', &mut self.line)
                .await;
            let ended = matches!(read, Ok(0) | Err(_));
            if ended {
                self.open = false;
            } else if self.line.last() != Some(&b'\n') && self.line.len() > MAX_LINE {
                self.line.clear();
                self.skipping = true;
                continue;
            } else if self.line.last() != Some(&b'\n') {
                continue;
            }
            let line = std::mem::take(&mut self.line);
            if std::mem::take(&mut self.skipping) || line.len() > MAX_LINE {
                return Err("frame exceeds the byte limit".into());
            }
            if line.is_empty() && ended {
                return Err("frame connection closed".into());
            }
            return serde_json::from_slice(&line).map_err(|e| format!("invalid frame: {e}"));
        }
        std::future::pending().await
    }
}

/// A session message, pushed after `listen{to:"session"}`.
pub fn message(cursor: &Cursor, message: &impl Serialize) -> Value {
    json!({"op": "message", "cursor": cursor, "message": message})
}

/// A question put to a listening script, answered by its reply to `id`.
pub fn ask(id: &Id, text: &str, options: &[String]) -> Value {
    json!({"id": id, "op": "ask", "text": text, "options": options})
}

/// A question that closed before it was answered.
pub fn refused(question: u64, why: &str) -> Value {
    json!({"op": "refused", "question": question, "message": why})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_what_makes_a_line_a_request() {
        let reply_needed: Incoming =
            serde_json::from_value(json!({"id":7,"op":"send","text":"hi"})).unwrap();
        assert_eq!(reply_needed.id(), Some(&Id::Number(7)));
        let notice: Incoming = serde_json::from_value(json!({"op":"send","text":"hi"})).unwrap();
        assert_eq!(notice.id(), None);
    }

    #[test]
    fn every_op_decodes_to_its_own_shape() {
        for (frame, want) in [
            (
                json!({"id":1,"op":"initialize","protocol":5,"client":"sensor"}),
                "initialize",
            ),
            (json!({"id":2,"op":"info"}), "info"),
            (json!({"op":"send","text":"/new"}), "send"),
            (
                json!({"op":"event","text":"tick","source":"timer","reason":"elapsed"}),
                "event",
            ),
            (json!({"op":"reply","to":"m-1","text":"yes"}), "reply"),
            (json!({"op":"status","text":"3/10"}), "status"),
            (json!({"op":"status"}), "status"),
            (json!({"op":"stop"}), "stop"),
            (json!({"id":"q","op":"listen","to":"questions"}), "listen"),
        ] {
            let frame: Incoming = serde_json::from_value(frame.clone()).unwrap();
            let got = match frame {
                Incoming::Initialize { .. } => "initialize",
                Incoming::Info { .. } => "info",
                Incoming::Send { .. } => "send",
                Incoming::Event { .. } => "event",
                Incoming::Reply { .. } => "reply",
                Incoming::Status { .. } => "status",
                Incoming::Stop {} => "stop",
                Incoming::Listen { .. } => "listen",
                other => panic!("{want}: {other:?}"),
            };
            assert_eq!(got, want);
        }
    }

    #[test]
    fn an_event_requires_source_and_reason() {
        let event: Incoming = serde_json::from_value(
            json!({"op":"event","text":"tick","source":"timer","reason":"elapsed"}),
        )
        .unwrap();
        assert!(event.validate().is_ok());
        let bare = serde_json::from_value::<Incoming>(json!({"op":"event","text":"tick"}));
        assert!(bare.is_err());
        let blank: Incoming = serde_json::from_value(
            json!({"op":"event","text":"tick","source":"","reason":"elapsed"}),
        )
        .unwrap();
        assert_eq!(blank.validate().unwrap_err().0, "frame");
    }

    #[test]
    fn the_old_frames_are_not_guessed_at() {
        for value in [
            json!({"type":"message","message":{}}),
            json!({"type":"runtime","command":{"type":"drive","enabled":true}}),
            json!({"type":"runtime","command":{"type":"command","line":"/new"}}),
            json!({"op":"say","text":"hi"}),
            json!({"op":"send","text":"hi","type":"runtime"}),
            json!({"op":"serve"}),
            json!({"op":"send","text":"hi","id":[]}),
        ] {
            assert!(
                serde_json::from_value::<Incoming>(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_reply_requires_text_or_cancel() {
        let empty: Incoming = serde_json::from_value(json!({"op":"reply","to":"m-1"})).unwrap();
        assert_eq!(empty.validate().unwrap_err().0, "frame");
        let cancelled: Incoming =
            serde_json::from_value(json!({"op":"reply","to":"m-1","cancel":true})).unwrap();
        assert!(cancelled.validate().is_ok());
    }
}
