//! What runs around each tool call — before it, a chance to refuse it; after
//! it, a note added to its result — what a run is told as it starts, what
//! keeps it from stopping, and what watches every event a run says.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tool::{Ctx, ToolOutput};

use crate::Event;

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

    /// A note for the model, filed before the run's first request; `prompt`
    /// is what the run was asked.
    async fn ahead(&self, _prompt: &str, _ctx: &Ctx) -> Option<String> {
        None
    }

    /// Asked when the model would stop with `reply`: `Some` keeps the run
    /// going, telling the model why. `held` counts the times this run was.
    async fn hold(&self, _reply: &str, _held: usize, _ctx: &Ctx) -> Option<String> {
        None
    }

    /// An event the run just said. Called on the loop itself, so it must
    /// not wait: hand the event off and return.
    fn saw(&self, _event: &Event) {}
}

/// Only the gates of the hooks it wraps: a child run's calls are checked as
/// its caller's are, but what it says reaches its caller as one tool result.
pub struct GatesOnly(pub Arc<dyn Hooks>);

#[async_trait]
impl Hooks for GatesOnly {
    async fn before(&self, tool: &str, args: &Value, ctx: &Ctx) -> Result<(), String> {
        self.0.before(tool, args, ctx).await
    }

    async fn after(&self, tool: &str, args: &Value, out: &ToolOutput, ctx: &Ctx) -> Option<String> {
        self.0.after(tool, args, out, ctx).await
    }
}

/// Nothing around any call.
pub struct NoHooks;

impl Hooks for NoHooks {}
