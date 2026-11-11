//! The sole fd 3 codec: one request or notice per line, one reply per request.
//!
//! A line carrying `id` is a request and is answered exactly once; a line
//! without it is a notice and is never answered. Both directions use the same
//! two shapes, which is what keeps one dispatch table enough.

use crate::runtime::channel::Cursor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_FRAME_BYTES: usize = message::DEFAULT_MESSAGE_BYTES + 4096;
pub const SESSION_VAR: &str = "PI_SESSION_ID";
pub const REQUEST_VAR: &str = "PI_REQUEST_ID";
/// The protocol revision this build speaks; a mismatch is refused, not guessed.
pub const PROTOCOL: u32 = 4;

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

/// Whose words these are: a person's line, or a machine's task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    #[default]
    Person,
    Task,
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
    },
    Say {
        #[serde(default)]
        id: Option<Id>,
        text: String,
        #[serde(rename = "as", default)]
        speaker: Speaker,
        #[serde(default)]
        note: String,
        #[serde(default)]
        more: bool,
        #[serde(default)]
        re: Option<message::MessageId>,
        #[serde(default)]
        stop: bool,
        #[serde(default)]
        source: String,
        #[serde(default)]
        reason: String,
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
        text: String,
    },
    Stop {},
}

impl Incoming {
    /// What only the frame's own contents can refuse, before it reaches pi.
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        match self {
            Self::Say {
                speaker: Speaker::Task,
                source,
                reason,
                ..
            } => {
                if source.trim().is_empty() || reason.trim().is_empty() {
                    return Err(("frame", "a task says where it comes from and why".into()));
                }
            }
            Self::Ask { text, .. } if text.trim().is_empty() => {
                return Err(("frame", "a question needs text".into()));
            }
            Self::Say {
                stop: true,
                re: None,
                ..
            } => {
                return Err(("frame", "stop names the message it closes".into()));
            }
            _ => {}
        }
        Ok(())
    }

    /// The request this line answers to, when it is one.
    pub fn id(&self) -> Option<&Id> {
        match self {
            Self::Initialize { id, .. } | Self::Listen { id, .. } => Some(id),
            Self::Say { id, .. } | Self::Ask { id, .. } => id.as_ref(),
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
            serde_json::from_value(json!({"id":7,"op":"say","text":"hi"})).unwrap();
        assert_eq!(reply_needed.id(), Some(&Id::Number(7)));
        let notice: Incoming = serde_json::from_value(json!({"op":"say","text":"hi"})).unwrap();
        assert_eq!(notice.id(), None);
    }

    #[test]
    fn every_op_decodes_to_its_own_shape() {
        for (frame, want) in [
            (json!({"id":1,"op":"initialize","protocol":4}), "initialize"),
            (json!({"op":"status","text":"3/10"}), "status"),
            (json!({"op":"stop"}), "stop"),
            (json!({"id":"q","op":"listen","to":"questions"}), "listen"),
        ] {
            let frame: Incoming = serde_json::from_value(frame.clone()).unwrap();
            let got = match frame {
                Incoming::Initialize { .. } => "initialize",
                Incoming::Status { .. } => "status",
                Incoming::Stop {} => "stop",
                Incoming::Listen { .. } => "listen",
                other => panic!("{want}: {other:?}"),
            };
            assert_eq!(got, want);
        }
    }

    #[test]
    fn a_task_must_say_where_it_came_from() {
        let task: Incoming = serde_json::from_value(
            json!({"op":"say","text":"tick","as":"task","source":"timer","reason":"elapsed"}),
        )
        .unwrap();
        assert!(task.validate().is_ok());
        let bare: Incoming =
            serde_json::from_value(json!({"op":"say","text":"tick","as":"task"})).unwrap();
        assert_eq!(bare.validate().unwrap_err().0, "frame");
    }

    #[test]
    fn the_old_frames_are_not_guessed_at() {
        for value in [
            json!({"type":"message","message":{}}),
            json!({"type":"runtime","command":{"type":"drive","enabled":true}}),
            json!({"type":"runtime","command":{"type":"command","line":"/new"}}),
            json!({"op":"say","text":"hi","type":"runtime"}),
            json!({"op":"serve"}),
            json!({"op":"say","text":"hi","id":[]}),
        ] {
            assert!(
                serde_json::from_value::<Incoming>(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_person_speaks_by_default() {
        let frame: Incoming = serde_json::from_value(json!({"op":"say","text":"/new"})).unwrap();
        let Incoming::Say { speaker, note, .. } = frame else {
            panic!("not a say")
        };
        assert_eq!(speaker, Speaker::Person);
        assert!(note.is_empty());
    }
}
