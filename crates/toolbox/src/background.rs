//! Lifecycle and controls for commands started by bash background.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

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
            Self::Tool { name, args } if args.as_object().is_some_and(|args| args.is_empty()) => {
                Cow::Borrowed(name)
            }
            Self::Tool { name, args } => Cow::Owned(format!("{name} {}", tool::arguments(args))),
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

/// A `Rerun` as data, for a pi started in this one's place, which has none
/// of this one's closures: a shell command and where it ran. Every execution
/// that can run again can say so both ways.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Again {
    pub command: String,
    pub cwd: PathBuf,
    pub stdin: Option<String>,
    pub environment: Environment,
    pub output: OutputMode,
}

/// Per-command script settings; environment-file values stay out of saved state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Environment {
    pub directory: Option<PathBuf>,
    pub arguments: Vec<(String, String)>,
}

/// Where a call that will outlive its turn is kept: an execution of the checkout at
/// `root`, which the host lists, stops through `stop`, and delivers from.
/// Session messages use the handle’s channel.
pub trait RunSink: Send + Sync {
    fn start(
        &self,
        root: PathBuf,
        invocation: Invocation,
        stop: CancellationToken,
    ) -> Arc<dyn RunHandle>;
}

/// Host-owned controls exposed through a shell. These add argument fields
/// and handle control requests; the shell remains the registered tool.
#[async_trait::async_trait]
pub trait Controls: Send + Sync {
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Map<String, serde_json::Value>;
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &tool::Ctx,
    ) -> Result<tool::ToolOutput, tool::ToolError>;
}

/// One running command's lifecycle and output.
pub trait RunHandle: Send + Sync {
    fn id(&self) -> u64;
    fn channel(&self) -> &dyn crate::channel::Channel;
    fn watch(&self, output: Peek);
    fn rerun(&self, _how: Rerun, _again: Again) {}
    fn end(&self);
}

/// Which captured streams accompany a successful command result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputMode {
    #[default]
    All,
    Stdout,
}
