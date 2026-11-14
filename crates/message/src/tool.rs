use crate::Image;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Call identity retained by its correlated result.
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call: String,
    /// The tool that actually ran, retained independently of the call identity.
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
