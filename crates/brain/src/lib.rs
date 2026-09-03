pub mod count;
pub mod model;
pub mod error;
pub mod estimate;
pub mod fault;
pub mod message;
pub mod request;
pub mod slice;
pub mod stream;
pub mod totals;
pub mod transport;

pub use model::{Format, ModelSpec, ReplayThinking};
pub use error::{BrainError, Result};
pub use fault::{Fault, classify};
pub use message::{
    AssistantContent, Message, ToolCall, ToolResult, UserContent,
};
pub use request::{Effort, Request, ToolChoice, ToolDef};
pub use stream::{Accumulator, Completion, StopReason, StreamEvent, Usage};
pub use totals::Totals;
pub use transport::Transport;
