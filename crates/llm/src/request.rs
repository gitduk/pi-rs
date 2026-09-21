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
    pub fn system_text(&self) -> Option<&str> {
        self.system.as_deref().or_else(|| {
            self.messages.iter().find_map(|m| match m {
                Message::System { content } => Some(content.as_str()),
                _ => None,
            })
        })
    }

    /// The temperature as the wires may send it: a non-finite value is a
    /// caller bug, and serde_json would spell it `null`, which providers reject.
    pub(crate) fn finite_temperature(&self) -> Option<f64> {
        self.temperature.filter(|t| t.is_finite())
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

    /// Kept separate from `as_openai`: the two vocabularies coincide today,
    /// and the split leaves each wire free to diverge without touching the
    /// other.
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
