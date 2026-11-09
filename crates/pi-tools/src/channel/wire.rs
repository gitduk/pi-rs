//! The sole fd 3 codec. Model messages and runtime operations are separate frames.

use super::{Asked, Cursor, Outcome, Response, Told};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_FRAME_BYTES: usize = message::DEFAULT_MESSAGE_BYTES + 4096;
pub const SESSION_VAR: &str = "PI_SESSION_ID";
pub const REQUEST_VAR: &str = "PI_REQUEST_ID";

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Incoming {
    Message {
        message: message::WireMessage,
    },
    Observe {
        session: message::SessionId,
        cursor: Option<Cursor>,
    },
    Runtime {
        command: Command,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status {
        text: String,
    },
    Command {
        line: String,
    },
    Prompt {
        text: String,
        options: Vec<String>,
        back: bool,
    },
    Answer {
        question: u64,
        picks: Vec<usize>,
    },
    AnswerText {
        question: u64,
        text: String,
    },
    Serve {},
    Interrupt {},
    Watch {},
    Drive {
        enabled: bool,
    },
}

impl Command {
    pub(super) fn asked(text: String, options: Vec<String>, back: bool) -> Asked {
        Asked {
            question: text,
            options,
            back,
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

pub fn error(message: impl Into<String>) -> Value {
    json!({"type":"error", "message":message.into()})
}

pub fn runtime(told: Told) -> Option<Value> {
    let event = match told {
        Told::Response { request, event } => operation(request, event),
        Told::Linked { operation, request } => {
            json!({"type":"linked", "operation":operation, "request":request})
        }
        Told::Started => json!({"type":"started"}),
        Told::Progress(text) => json!({"type":"progress", "text":text}),
        Told::Reply(text) => json!({"type":"finished", "text":text, "status":{"type":"done"}}),
        Told::Asked => json!({"type":"prompted"}),
        Told::Answer(Ok(text)) => json!({"type":"answer", "text":text}),
        Told::Answer(Err(message)) => json!({"type":"error", "message":message}),
        Told::Question {
            id,
            asked,
            request: None,
        } => {
            json!({"type":"prompt", "question":id, "text":asked.question, "options":asked.options, "back":asked.back})
        }
        Told::Question {
            request: Some(_), ..
        } => return None,
        Told::Closed(question) => json!({"type":"closed", "question":question}),
        Told::Refused { question, why } => {
            json!({"type":"refused", "question":question, "message":why})
        }
    };
    Some(json!({"type":"runtime", "event":event}))
}

fn operation(request: u64, event: Response) -> Value {
    let phase = match event {
        Response::Queued => json!({"type":"queued"}),
        Response::Started => json!({"type":"started"}),
        Response::Progress(text) => json!({"type":"progress", "text":text}),
        Response::Finished { text, outcome } => {
            let (status, exit) = match outcome {
                Outcome::Done => (json!({"type":"done"}), None),
                Outcome::Stopped(reason) => (json!({"type":"stopped", "reason":reason}), None),
                Outcome::Failed(error) => (
                    json!({"type":"failed", "failure":{"code":"OPERATION_FAILED", "message":error}}),
                    None,
                ),
                Outcome::Exited(code) => (
                    if code == 0 {
                        json!({"type":"done"})
                    } else {
                        json!({"type":"failed", "failure":{"code":"PROCESS_EXIT", "message":format!("exited {code}")}})
                    },
                    Some(code),
                ),
            };
            json!({"type":"finished", "text":text, "status":status, "exit_code":exit})
        }
    };
    json!({"type":"operation", "operation":request, "phase":phase})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_and_mixed_frames_are_rejected_without_guessing() {
        for value in [
            json!({"input":"hello"}),
            json!({"result":"finished"}),
            json!({"follow":true}),
            json!({"choose":"allow?", "options":["yes","no"]}),
            json!({"type":"runtime", "command":{"type":"interrupt"}, "input":"hello"}),
            json!({"type":"runtime", "command":{"type":"interrupt", "enabled":true}}),
            json!({"type":"runtime", "command":{"type":"follow"}}),
        ] {
            assert!(
                serde_json::from_value::<Incoming>(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn semantic_content_cannot_silently_carry_runtime_fields() {
        let message = json!({
            "id":"input", "session":"session", "at":1,
            "kind":{"type":"request","value":{"direction":"to_model","request":"user"}},
            "body":[{"type":"text","value":"/new"}]
        });
        let frame = json!({"type":"message", "message":message});
        assert!(serde_json::from_value::<Incoming>(frame.clone()).is_ok());
        for pointer in ["/message/kind", "/message/kind/value", "/message/body/0"] {
            let mut mixed = frame.clone();
            mixed
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("interrupt".into(), json!(true));
            assert!(
                serde_json::from_value::<Incoming>(mixed.clone()).is_err(),
                "{mixed}"
            );
        }
    }
}
