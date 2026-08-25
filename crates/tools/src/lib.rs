use async_trait::async_trait;
use brain::message::ToolResultContent;
use serde_json::{Deserializer, Value};

/// Parse a tool's arguments with a serde path on any error, so a missing or
/// misspelled field says which one (`items[1].status`) instead of a bare
/// `missing field 'status'` that could be any of a hundred places.
pub fn parse_args<T>(args: Value) -> Result<T, serde_json::Error>
where
    T: serde::de::DeserializeOwned,
{
    let text = args.to_string();
    let mut de = Deserializer::from_str(&text);
    serde_path_to_error::deserialize(&mut de).map_err(|e| serde::de::Error::custom(e.to_string()))
}

pub mod bash;
pub mod blocks;
pub mod edit;
pub mod finish;
pub mod glob;
pub mod grep;
mod parses;
pub mod read;
pub mod registry;
mod rows;
pub mod skill;
pub mod skills;
pub mod spill;
pub mod todo;
pub mod walk;
pub mod workspace;
pub mod write;

pub use registry::Registry;
pub use workspace::Workspace;
pub mod state;

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

    /// The command ran past its deadline and the process group was killed.
    #[error("timed out after {ms}ms; the command and everything it spawned were killed")]
    Timeout { ms: u64 },

    /// An over-long output could not be persisted for later reading.
    #[error("could not spill oversized output: {0}")]
    Spill(String),
}

impl ToolError {
    /// A stable code the model can branch on, where one exists. The loop
    /// appends it to the result as `[code: {code}]`; codes never change for a
    /// given failure, whatever the prose says.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            ToolError::Timeout { .. } => Some("TOOL_TIMEOUT"),
            ToolError::Spill(_) => Some("SPILL_FAILED"),
            _ => None,
        }
    }
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
    /// The session this context runs in. None in tests and for embedders;
    /// spills then land in the process temp directory.
    session: Option<String>,
    /// Where over-long outputs land, `<root>/<session>/<n>.log`.
    spill_root: std::path::PathBuf,
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
            session: None,
            spill_root: spill::default_root(None),
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

    /// Name the session, and with it the directory spills are kept in. A
    /// resumed session keeps its id, so its new spills rejoin the old ones.
    pub fn with_session(mut self, id: impl Into<String>) -> Self {
        let ns = state::file_stem(&id.into());
        self.session = Some(ns.clone());
        self.spill_root = spill::default_root(Some(&ns));
        self
    }

    /// Where spills land, overriding the session default. Tests and alternate
    /// hosts point this at a directory of their own instead of the user's.
    pub fn with_spill_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.spill_root = root.into();
        self
    }

    /// The namespace new spills are filed under.
    pub fn spill_namespace(&self) -> &str {
        self.session.as_deref().unwrap_or("default")
    }

    /// Resolve a `spill:` locator to the file it names. The workspace gate is
    /// deliberately not applied: spill files live outside the workspace, and
    /// only locators of the shape our own writer mints are accepted.
    pub fn spill_path(&self, locator: &str) -> Result<std::path::PathBuf, ToolError> {
        spill::locate(&self.spill_root, locator)
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
