pub mod catalog;
pub mod error;
pub mod estimate;
pub mod message;
pub mod request;
pub mod stream;
pub mod transport;

pub use catalog::{Capabilities, ModelSpec, ThinkingReplay, Wire};
pub use error::{BrainError, Result};
pub use message::{
    AssistantContent, Message, ProviderCallId, ToolCall, ToolCallId, ToolResult, UserContent,
};
pub use request::{Effort, Request, ToolChoice, ToolDef};
pub use stream::{Accumulator, Completion, StopReason, StreamEvent, Usage};
pub use transport::Transport;
