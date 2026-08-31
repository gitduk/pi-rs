use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::Message;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Abstract reasoning intensity. Transports translate: Anthropic gets a token
/// budget, OpenAI-family an effort string. Never store a wire value here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    /// True this turn only. Rides the tail of the request and never enters the
    /// session, so a note cannot be read back next turn as a statement of fact
    /// — which is what a turn counter written into the transcript becomes.
    ///
    /// The tail is also the one place it is free: everything before it is
    /// unchanged from last turn, so the cached prefix still reaches as far as
    /// it did.
    ///
    /// Nothing produces one today. The plan did, and that is exactly what went
    /// wrong: appended to the user's turn it was indistinguishable from the
    /// user speaking, so a stale list got argued with instead of updated. The
    /// mechanism is kept because per-turn state that must not be read back as
    /// fact is a real need — but whatever fills it next has to survive being
    /// read in the user's voice, because that is how it will be read.
    pub notes: Vec<String>,
    pub tools: Vec<ToolDef>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub effort: Effort,
    pub tool_choice: ToolChoice,
}

impl Request {
    /// The system prompt this request sends: the explicit field, else the
    /// first system message in the transcript. Every transport puts it on the
    /// wire in its own place, but all three resolve it the same way.
    pub fn system_text(&self) -> Option<String> {
        self.system.clone().or_else(|| {
            self.messages.iter().find_map(|m| match m {
                Message::System { content } => Some(content.clone()),
                _ => None,
            })
        })
    }
}
impl Effort {
    /// Fraction of the output budget handed to thinking. Anthropic requires the
    /// budget to stay below max_tokens, so this is a ratio, not a constant.
    pub fn budget_ratio(self) -> Option<f64> {
        match self {
            Effort::Off => None,
            Effort::Low => Some(0.25),
            Effort::Medium => Some(0.5),
            Effort::High => Some(0.8),
        }
    }

    /// Kept separate from `as_openai`: the vocabularies have already diverged —
    /// Anthropic adds `xhigh`/`max`, OpenAI `none`/`minimal`.
    pub fn as_anthropic(self) -> Option<&'static str> {
        match self {
            Effort::Off => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
        }
    }

    pub fn as_openai(self) -> Option<&'static str> {
        match self {
            Effort::Off => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
        }
    }
}
