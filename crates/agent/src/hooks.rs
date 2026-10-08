//! What runs around each tool call: before it, a chance to refuse it; after
//! it, a note added to its result.

use async_trait::async_trait;
use serde_json::Value;
use tool::{Ctx, ToolOutput};

#[async_trait]
pub trait Hooks: Send + Sync {
    /// `Err` refuses the call, carrying what the model is told instead.
    async fn before(&self, _tool: &str, _args: &Value, _ctx: &Ctx) -> Result<(), String> {
        Ok(())
    }

    /// A note to add to the call's result.
    async fn after(
        &self,
        _tool: &str,
        _args: &Value,
        _out: &ToolOutput,
        _ctx: &Ctx,
    ) -> Option<String> {
        None
    }
}

/// Nothing around any call.
pub struct NoHooks;

impl Hooks for NoHooks {}
