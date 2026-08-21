use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Our own correlation handle for a tool call. Always present, minted locally
/// when the provider issued no identifier of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

/// What the provider issued. Only this may travel back on that provider's wire;
/// replaying it to a different provider is a protocol error.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderCallId(pub String);

impl ToolCallId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ProviderCallId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which transport and model produced a block. Read before replaying anything
/// opaque (reasoning signatures, encrypted items) to a different model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub transport: String,
    pub model: String,
}

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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
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
    pub id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderCallId>,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderCallId>,
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
    /// Absent means locally synthesized; a mismatch against the target model is
    /// what triggers demotion instead of replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
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

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            content: vec![UserContent::Text(Text { text: text.into() })],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Message::Assistant {
            id: None,
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
    pub fn text(call: ToolCallId, name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            call,
            provider: None,
            name: name.into(),
            content: vec![ToolResultContent::Text(Text { text: body.into() })],
            is_error: false,
            useless: false,
        }
    }

    pub fn error(call: ToolCallId, name: impl Into<String>, body: impl Into<String>) -> Self {
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
