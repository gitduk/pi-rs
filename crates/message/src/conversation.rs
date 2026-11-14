//! The four kinds of content retained in a conversation.

use crate::{Block, ToolCall, ToolResult};
use serde::{Deserialize, Serialize};

/// Ordered content. Reply grouping and context selection belong to the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "content", rename_all = "snake_case")]
pub enum Content {
    User(Vec<Block>),
    Assistant(Vec<Block>),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}
