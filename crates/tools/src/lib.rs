use async_trait::async_trait;
use brain::message::ToolResultContent;
use serde_json::Value;

pub mod bash;
pub mod blocks;
pub mod edit;
pub mod finish;
pub mod glob;
pub mod grep;
pub mod read;
pub mod registry;
mod rows;
pub mod skill;
pub mod skills;
pub mod todo;
pub mod walk;
pub mod workspace;
pub mod write;

pub use registry::Registry;
pub use workspace::Workspace;

/// What a call is permitted to touch. The approval gate reads this; it is a
/// static classification, not a guess about any particular argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

/// How a call schedules against the other calls in the same turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    Shared,
    /// Runs alone; every other call in the turn waits.
    Exclusive,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Invalid(String),

    /// The one failure the loop must not hand back to the model.
    #[error("cancelled")]
    Cancelled,

    #[error("path escapes the workspace: {0}")]
    Escape(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: Vec<ToolResultContent>,
    /// Carries no information for later turns. Compaction may drop it.
    pub useless: bool,
    /// One line for a progress display. Set it when the first line of the
    /// result is structure rather than content.
    pub preview: Option<String>,
}

impl ToolOutput {
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(brain::message::Text {
                text: body.into(),
            })],
            useless: false,
            preview: None,
        }
    }

    pub fn with_preview(mut self, line: impl Into<String>) -> Self {
        self.preview = Some(line.into());
        self
    }

    /// Falls back to the first line of the result, which is right for tools
    /// whose result opens with content rather than a marker.
    pub fn preview(&self) -> String {
        match &self.preview {
            Some(p) => p.clone(),
            None => self
                .flatten()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
        }
    }

    pub fn useless(body: impl Into<String>) -> Self {
        Self {
            useless: true,
            ..Self::text(body)
        }
    }

    pub fn flatten(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text(t) => t.text.clone(),
                ToolResultContent::Json { value } => value.to_string(),
                ToolResultContent::Image(_) => "[image]".into(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub workspace: Workspace,
    pub cancel: tokio_util::sync::CancellationToken,
    /// The agent's plan. Shared so the tool can write it and the loop can record
    /// it into the session, without the tool knowing a session exists.
    pub todos: std::sync::Arc<std::sync::Mutex<Vec<todo::Todo>>>,
    /// Where `yield` leaves the run's result, when a schema was asked for.
    pub yielded: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    /// One lock per file. Tools in the same turn run concurrently, and two
    /// writers to one path otherwise read the same bytes, both succeed, and
    /// one change vanishes without anyone being told.
    pub file_locks: FileLocks,
}

pub type FileLocks =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, FileLock>>>;

pub type FileLock = std::sync::Arc<tokio::sync::Mutex<()>>;

impl Ctx {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            cancel: tokio_util::sync::CancellationToken::new(),
            todos: Default::default(),
            yielded: Default::default(),
            file_locks: Default::default(),
        }
    }
}

impl Ctx {
    /// Swap in a cancellation token. A caller that runs many turns wants a
    /// fresh one each time while the shared handles carry over.
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// A fresh slot for a run's structured result, leaving everything else.
    pub fn with_fresh_result(mut self) -> Self {
        self.yielded = Default::default();
        self
    }

    /// Hold this while mutating `path`. Keyed on the resolved path, so two
    /// spellings of one file serialize together.
    pub async fn lock_file(&self, path: &std::path::Path) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.file_locks.lock().expect("file locks poisoned");
            map.entry(path.to_path_buf()).or_default().clone()
        };
        lock.lock_owned().await
    }
}

/// A `ToolError` is not fatal: the loop turns it into an error result the model
/// reads and retries against. Only cancellation ends a turn.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    fn tier(&self) -> Tier;

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError>;
}
