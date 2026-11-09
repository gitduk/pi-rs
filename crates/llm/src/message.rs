use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Format, ModelSpec};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System { content: String },
    User { content: Vec<UserContent> },
    Assistant { content: Vec<AssistantContent> },
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    // Literal text. Transports must not reinterpret it as structured JSON.
    Text(Text),
    Image(Image),
    Json { value: Value },
}

pub use message::{Image, Reasoning, ReasoningContent};

/// How a stored reasoning block goes back out to one model.
///
/// Decided once, so transports and the token estimate can't disagree: an
/// outrun count compacts bytes that never leave; a lagging one risks a 400.
pub enum Replay<'a> {
    // The target's own text, with its signature when the endpoint supplied one.
    Thinking { signature: Option<&'a str> },
    // The target's own, encrypted: the ciphertext is the whole of what goes.
    Encrypted { id: &'a str, encrypted: &'a str },
    // The target's own, sealed by the provider before it reached us: opaque,
    // with no id, and owed back as it came.
    Redacted { data: &'a str },
    // The target's own, as the chat wire carries thinking: prose beside the
    // content, unsigned, so only the block its author wrote goes back.
    Prose { text: String },
    // Nothing leaves.
    Dropped,
}

pub fn replay_for<'a>(reasoning: &'a Reasoning, spec: &ModelSpec) -> Replay<'a> {
    if reasoning.by.as_deref() == Some(spec.model.as_str()) {
        match spec.format {
            Format::Anthropic { .. } => {
                let signature = reasoning.content.iter().find_map(|c| match c {
                    ReasoningContent::Text { signature, .. } => signature.as_deref(),
                    ReasoningContent::Encrypted(_) => None,
                });
                if reasoning
                    .content
                    .iter()
                    .any(|c| matches!(c, ReasoningContent::Text { .. }))
                {
                    return Replay::Thinking { signature };
                }
                if let Some(data) = reasoning.content.iter().find_map(|c| match c {
                    ReasoningContent::Encrypted(s) => Some(s.as_str()),
                    ReasoningContent::Text { .. } => None,
                }) {
                    return Replay::Redacted { data };
                }
            }
            Format::OpenAi => {
                let encrypted = reasoning.content.iter().find_map(|c| match c {
                    ReasoningContent::Encrypted(s) => Some(s.as_str()),
                    ReasoningContent::Text { .. } => None,
                });
                // `id` and the ciphertext are both required, or the
                // endpoint refuses the item — missing either demotes.
                if let (Some(id), Some(encrypted)) = (reasoning.id.as_deref(), encrypted) {
                    return Replay::Encrypted { id, encrypted };
                }
            }
            // With tools in play the chat wire requires its thinking back,
            // or the model loses the thread between sub-turns.
            Format::Chat => {
                let text = reasoning.text();
                if !text.is_empty() {
                    return Replay::Prose { text };
                }
            }
        }
    }

    // Foreign or without a shape this format can replay: left out.
    Replay::Dropped
}

/// Every image swapped for a line saying why, for a model that takes none:
/// one sent anyway fails the whole request.
pub fn blind(messages: &mut [Message], model: &str) {
    let note = || Text {
        text: format!("[image not shown: {model} does not take images]"),
    };
    for message in messages {
        let Message::User { content } = message else {
            continue;
        };
        for part in content {
            match part {
                UserContent::Image(_) => *part = UserContent::Text(note()),
                UserContent::ToolResult(result) => {
                    for part in &mut result.content {
                        if matches!(part, ToolResultContent::Image(_)) {
                            *part = ToolResultContent::Text(note());
                        }
                    }
                }
                UserContent::Text(_) => {}
            }
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
        }
    }

    pub fn error(
        call: impl Into<String>,
        name: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            is_error: true,
            ..Self::text(call, name, body)
        }
    }

    /// Flattened text for wires that accept only a string result body.
    pub fn flatten_text(&self) -> String {
        flatten(&self.content)
    }
}

/// A result's content as one text: JSON as written, an image as `[image]`.
pub fn flatten(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(t) => t.text.clone(),
            ToolResultContent::Json { value } => value.to_string(),
            ToolResultContent::Image(_) => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
