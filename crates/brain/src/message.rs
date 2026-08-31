use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Format, ModelSpec, ReplayThinking};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: Vec<UserContent>,
    },
    Assistant {
        content: Vec<AssistantContent>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
    Text(Text),
    ToolResult(ToolResult),
    Image(Image),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    Text(Text),
    ToolCall(ToolCall),
    Reasoning(Reasoning),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// What the provider called it, or a local `call_N` when it named none.
    /// Either way it is what travels back as the result's `call`.
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call: String,
    /// The tool that actually ran. Required: Gemini's `functionResponse.name`
    /// and Ollama's tool messages key replay on it, and an id is not a name.
    pub name: String,
    pub content: Vec<ToolResultContent>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Carries no information for later turns (zero matches, wait timeout).
    /// Compaction may drop it once consumed. Ignored when `is_error`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub useless: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    /// Literal text. Transports must not reinterpret it as structured JSON.
    Text(Text),
    Image(Image),
    Json {
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Image {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub content: Vec<ReasoningContent>,
    /// Which model produced it, as the endpoint names it. Absent means locally
    /// synthesized; anything but an exact match against the target is demoted
    /// rather than replayed, because a signature or a ciphertext is only ever
    /// readable by the model that made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ReasoningContent {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Encrypted(String),
}

/// How a stored reasoning block goes back out to one model.
///
/// Both transports and the token estimate need this answer, and each used to
/// derive it for itself — a count that outruns the encoder compacts against
/// bytes that never leave, and one that lags it walks into a 400. Decided here
/// once, they cannot disagree.
pub enum Replay<'a> {
    /// The target's own, signed: the block replays as itself.
    Signed { signature: &'a str },
    /// The target's own, encrypted: the ciphertext is the whole of what goes.
    Encrypted { id: &'a str, encrypted: &'a str },
    /// Foreign or unsigned, and the target reads `<think>`: demoted to prose.
    Demoted,
    /// Nothing leaves.
    Dropped,
}

/// The wrapper a demoted block ships inside. Written once so the two
/// transports and the estimate's `TAG_OVERHEAD` describe the same bytes.
pub fn tagged(text: &str) -> String {
    format!("<think>\n{text}\n</think>")
}

impl Reasoning {
    /// The prose halves, joined — all a demoted block ever shows the target.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ReasoningContent::Text { text, .. } => Some(text.as_str()),
                ReasoningContent::Encrypted(_) => None,
            })
            .collect()
    }

    /// Which way this block leaves for `spec`.
    ///
    /// Keyed on the target's format rather than on the calling transport,
    /// because the estimate has no transport: the two agree only because the
    /// transport is itself chosen by that same field.
    pub fn replay_for(&self, spec: &ModelSpec) -> Replay<'_> {
        if self.by.as_deref() == Some(spec.model.as_str()) {
            match spec.format {
                Format::Anthropic { .. } => {
                    let signature = self.content.iter().find_map(|c| match c {
                        ReasoningContent::Text { signature, .. } => signature.as_deref(),
                        ReasoningContent::Encrypted(_) => None,
                    });
                    if let Some(signature) = signature {
                        return Replay::Signed { signature };
                    }
                }
                Format::OpenAi => {
                    let encrypted = self.content.iter().find_map(|c| match c {
                        ReasoningContent::Encrypted(s) => Some(s.as_str()),
                        ReasoningContent::Text { .. } => None,
                    });
                    // `id` is required on the item and the ciphertext is the
                    // whole of what replays, so a block missing either demotes
                    // rather than shipping an item the endpoint refuses.
                    if let (Some(id), Some(encrypted)) = (self.id.as_deref(), encrypted) {
                        return Replay::Encrypted { id, encrypted };
                    }
                }
                // Chat has no signed or encrypted reasoning either: the wire
                // carries thinking as plain `reasoning_content`, which is not
                // a signed/encrypted shape, so even the model's own blocks
                // fall through to the replay_thinking policy below.
                Format::Chat => {}
            }
        }

        let has_text = self
            .content
            .iter()
            .any(|c| matches!(c, ReasoningContent::Text { text, .. } if !text.is_empty()));
        match spec.replay_thinking {
            ReplayThinking::Tagged if has_text => Replay::Demoted,
            _ => Replay::Dropped,
        }
    }
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            content: vec![UserContent::Text(Text { text: text.into() })],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Message::Assistant {
            content: vec![AssistantContent::Text(Text { text: text.into() })],
        }
    }

    /// One user message carrying every result of the preceding assistant turn.
    /// Wires that want one message per result split this; the reverse does not
    /// reassemble, so this is the shape worth storing.
    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        Message::User {
            content: results.into_iter().map(UserContent::ToolResult).collect(),
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        let blocks: &[AssistantContent] = match self {
            Message::Assistant { content, .. } => content,
            _ => &[],
        };
        blocks.iter().filter_map(|b| match b {
            AssistantContent::ToolCall(c) => Some(c),
            _ => None,
        })
    }

    pub fn text(&self) -> String {
        match self {
            Message::System { content } => content.clone(),
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect(),
            Message::Assistant { content, .. } => content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect(),
        }
    }
}

impl ToolResult {
    pub fn text(call: impl Into<String>, name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            call: call.into(),
            name: name.into(),
            content: vec![ToolResultContent::Text(Text { text: body.into() })],
            is_error: false,
            useless: false,
        }
    }

    pub fn error(call: impl Into<String>, name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(call, name, body)
        }
    }

    /// Flattened text for wires that accept only a string result body.
    pub fn flatten_text(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text(t) => t.text.clone(),
                ToolResultContent::Json { value } => value.to_string(),
                ToolResultContent::Image(_) => "[image]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
