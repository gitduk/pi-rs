//! Lifecycle and controls for executions that can outlive a calling turn.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument as _;

pub use crate::shell::{Again, Controls, Environment, OutputMode};

/// The original call, independent of the command prepared to execute it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Invocation {
    Shell {
        command: String,
    },
    Tool {
        name: String,
        args: serde_json::Value,
    },
    /// An execution started by a module rather than a tool invocation.
    Task {
        name: String,
        description: String,
    },
}

impl Invocation {
    pub fn shell(command: impl Into<String>) -> Self {
        Self::Shell {
            command: command.into(),
        }
    }

    /// A single-line description of the original call, without width limits.
    pub fn title(&self) -> Cow<'_, str> {
        match self {
            Self::Shell { command } => Cow::Borrowed(command.lines().next().unwrap_or_default()),
            Self::Tool { name, args } => tool::title(name, args),
            Self::Task { name, description } => Cow::Owned(format!("{name}: {description}")),
        }
    }
}

/// Reads the latest of what a running execution has printed, on demand.
#[derive(Clone)]
pub struct Peek(Arc<dyn Fn(usize) -> Printed + Send + Sync>);

/// What an execution printed after a byte count it reported before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Printed {
    pub text: String,
    /// Bytes printed so far, to read on from next time.
    pub total: usize,
    /// Part of what came after the count is gone: `text` starts at a later line.
    pub cut: bool,
}

impl Peek {
    /// `read(from)` answers with what was printed after `from` bytes.
    pub fn new(read: impl Fn(usize) -> Printed + Send + Sync + 'static) -> Self {
        Self(Arc::new(read))
    }

    /// Text that is all there is: read once more, it holds nothing new.
    pub fn text(text: String) -> Self {
        Self::new(move |from| Printed {
            text: text.get(from..).unwrap_or_default().to_string(),
            total: text.len(),
            cut: false,
        })
    }

    /// The latest of it, as much as is kept.
    pub fn latest(&self) -> String {
        self.since(0).text
    }

    pub fn since(&self, from: usize) -> Printed {
        (self.0)(from)
    }
}

/// Starts an execution over: runs it again, reporting through the handle it is
/// given and wound down by the token, as the first run was.
pub type Rerun =
    Arc<dyn Fn(Arc<dyn RunHandle>, CancellationToken) -> Result<(), tool::ToolError> + Send + Sync>;

/// The checkout and original conversation a command belongs to. A command
/// started without a conversation can serve its checkout as a runtime service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub root: PathBuf,
    pub owner: Option<Owner>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Owner {
    pub session: String,
    pub request: Option<message::MessageId>,
}

impl From<&tool::Ctx> for Scope {
    fn from(ctx: &tool::Ctx) -> Self {
        let owner = match ctx.request() {
            Some(request) => Some(Owner {
                session: request.session().as_str().to_owned(),
                request: Some(request.id().clone()),
            }),
            None => ctx.session().map(|session| Owner {
                session: session.to_owned(),
                request: None,
            }),
        };
        Self {
            root: ctx.workspace.root().to_path_buf(),
            owner,
        }
    }
}

impl From<PathBuf> for Scope {
    fn from(root: PathBuf) -> Self {
        Self { root, owner: None }
    }
}

impl From<&std::path::Path> for Scope {
    fn from(root: &std::path::Path) -> Self {
        root.to_path_buf().into()
    }
}

impl From<&str> for Scope {
    fn from(root: &str) -> Self {
        PathBuf::from(root).into()
    }
}

/// Where a call that will outlive its turn is kept: an execution of the checkout at
/// `root`, which the host lists, stops through `stop`, and delivers from.
/// Session messages use the handle’s channel.
pub trait RunSink: Send + Sync {
    fn start(
        &self,
        scope: Scope,
        invocation: Invocation,
        stop: CancellationToken,
    ) -> Arc<dyn RunHandle>;
}

/// One running execution's lifecycle and output.
pub trait RunHandle: Send + Sync {
    fn id(&self) -> u64;
    fn owner(&self) -> Option<Owner> {
        None
    }
    fn channel(&self) -> &dyn crate::channel::Channel;
    fn watch(&self, output: Peek);
    fn rerun(&self, _how: Rerun, _again: Again) {}
    fn end(&self);
    /// Track owned work until its cleanup and terminal report have completed.
    fn tracker(&self) -> Option<TaskTracker> {
        None
    }
    /// Publish a terminal report. Explicit stops do not request another turn.
    /// Hosts can make this operation idempotent and retain structured facts.
    fn complete(&self, report: Report) {
        if report.outcome == Outcome::Cancelled {
            return;
        }
        self.channel().result(report.text());
        self.end();
    }
}

/// How an execution ended, separate from its human-readable output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Incomplete,
    Failed,
    Cancelled,
}

/// A terminal result. Usage is information, not an instruction to charge again.
pub struct Report {
    pub outcome: Outcome,
    pub output: tool::ToolOutput,
}

impl Report {
    /// Render a terminal result for hosts with a text result channel.
    pub fn text(&self) -> String {
        self.output.flatten()
    }

    /// Preserve terminal state and usage when admitting the result to a session.
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "outcome": self.outcome,
            "usage": self.output.spent,
            "warned": self.output.warned,
        })
    }
}

/// Drive an owned execution and report failure if it unwinds or is dropped.
pub fn spawn(
    execution: Arc<dyn RunHandle>,
    running: impl std::future::Future<Output = Report> + Send + 'static,
) {
    // Construct before spawning: even a task dropped before its first poll ends.
    let tracked = execution.tracker().map(|tracker| tracker.token());
    let guard = CompletionGuard(Some(execution));
    tokio::spawn(
        async move {
            let _tracked = tracked;
            let mut guard = guard;
            let report = running.await;
            if let Some(execution) = guard.0.take() {
                execution.complete(report);
            }
        }
        .in_current_span(),
    );
}

struct CompletionGuard(Option<Arc<dyn RunHandle>>);

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(execution) = self.0.take() {
            execution.complete(Report {
                outcome: Outcome::Failed,
                output: tool::ToolOutput::text("execution ended without a terminal report")
                    .warned(),
            });
        }
    }
}
