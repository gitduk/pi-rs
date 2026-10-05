use async_trait::async_trait;
use llm::message::ToolResultContent;
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

/// Parse a tool's arguments; on failure, append `hint` after the serde error
/// so the model sees what the tool takes, not just which field it missed.
pub fn parse_args_hinted<T>(args: Value, hint: &str) -> Result<T, ToolError>
where
    T: serde::de::DeserializeOwned,
{
    parse_args(args).map_err(|e| ToolError::Invalid(format!("json: {e} — {hint}")))
}

pub mod limit;
pub mod output;
pub mod registry;
pub mod spill;
pub mod state;
pub mod workspace;

pub use registry::{Registry, Source};
pub use workspace::Workspace;

/// What a call is permitted to touch. The approval gate reads this; it is a
/// static classification, not a guess about any particular argument.
///
/// Not a ladder, which is why there is no `Ord`: `Read`/`Write`/`Exec` nest,
/// but `Net` is a different direction — reaching the web changes nothing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Read,
    Write,
    Exec,
    Net,
}

impl Tier {
    /// Whether a run capped at `ceiling` may make a call of this tier.
    ///
    /// `Exec` covers `Net` because `sh` can `curl`. Refusing the fetch tool to
    /// a run that may spawn a shell would deny nothing and teach the model to
    /// route around the tool that reports its failures.
    pub fn under(self, ceiling: Tier) -> bool {
        match (ceiling, self) {
            (Tier::Exec, _) => true,
            (Tier::Write, t) => matches!(t, Tier::Read | Tier::Write),
            (Tier::Net, t) => matches!(t, Tier::Read | Tier::Net),
            (Tier::Read, t) => matches!(t, Tier::Read),
        }
    }

    /// Whether a path this tier resolves is held inside the workspace. Reading
    /// may name anything on the machine; changing it or running in it may not.
    ///
    /// A `match`, not `!= Read`: a tier added later fails to compile until
    /// someone decides which side it belongs on.
    pub fn fenced(self) -> bool {
        match self {
            Tier::Read => false,
            Tier::Write | Tier::Exec => true,
            // Never asked — nothing at this tier opens a path. If that ever
            // changes, inside the workspace is the answer to start from.
            Tier::Net => true,
        }
    }
}

#[cfg(test)]
mod tier_tests {
    use super::Tier::{self, Exec, Net, Read, Write};

    const ALL: [Tier; 4] = [Read, Write, Exec, Net];

    // The compiler can't check `ALL` against the enum, but this can: a fifth
    // variant makes this match non-exhaustive, forcing it to be added here too.
    fn _every_tier_is_in_all(t: Tier) {
        match t {
            Read | Write | Exec | Net => assert!(ALL.contains(&t)),
        }
    }

    // Why `Net` is a tier, not a rung: reaching the web must not silently
    // grant the right to change anything, nor the reverse.
    #[test]
    fn the_tier_lattice_reaches_as_documented() {
        let reach = |c: Tier| ALL.into_iter().filter(|t| t.under(c)).collect::<Vec<_>>();
        assert_eq!(reach(Read), vec![Read]);
        assert_eq!(reach(Write), vec![Read, Write]);
        assert_eq!(reach(Net), vec![Read, Net]);
        assert_eq!(reach(Exec), vec![Read, Write, Exec, Net]);

        assert!(!Net.under(Write));
        assert!(!Write.under(Net));
        assert!(!Net.under(Read));
        assert!(Net.under(Exec));
    }
}

/// How a call schedules against the other calls in the same turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    Shared,
    // Runs alone; every other call in the turn waits.
    Exclusive,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    // A refusal whose prose the model reads.
    #[error("{0}")]
    Invalid(String),

    // The one failure the loop must not hand back to the model.
    #[error("cancelled")]
    Cancelled,

    #[error("path escapes the workspace: {0}")]
    Escape(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("timed out after {ms}ms; the command and everything it spawned were killed")]
    Timeout { ms: u64 },

    #[error("could not spill oversized output: {0}")]
    Spill(String),
}

impl ToolError {
    /// The stable code the model can branch on, where one exists. The loop
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
    /// One line for a progress display. Set it when the first line of the
    /// result is structure rather than content.
    pub preview: Option<String>,
    /// What the call spent, for a tool that runs a model of its own. Tokens
    /// only: what they are worth is the surface's arithmetic, not ours.
    pub spent: llm::stream::Usage,
}

impl ToolOutput {
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(llm::message::Text {
                text: body.into(),
            })],
            preview: None,
            spent: llm::stream::Usage::default(),
        }
    }

    pub fn with_preview(mut self, line: impl Into<String>) -> Self {
        self.preview = Some(line.into());
        self
    }

    /// Report what this call used beyond its own turn, for the caller's run
    /// to count.
    pub fn with_spent(mut self, spent: llm::stream::Usage) -> Self {
        self.spent = spent;
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
    /// One lock per file. Tools in the same turn run concurrently, and two
    /// writers to one path otherwise read the same bytes, both succeed, and
    /// one change vanishes without anyone being told.
    pub file_locks: FileLocks,
    /// The content hash each file's last view was built on. Feeds the
    /// staleness note and the read-before-edit rule.
    pub viewed: Viewed,
    // Paths this run has changed. Split from a cloned parent's rather than
    // shared: this answers what *this* run did, not what the whole tree got.
    writes: std::sync::Arc<std::sync::Mutex<Written>>,
    // The session this context runs in. None in tests and for embedders;
    // spills then land in the process temp directory.
    session: Option<String>,
    // Where over-long outputs land, `<root>/<session>/<n>.log`.
    spill_root: std::path::PathBuf,
}

pub type FileLocks =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, FileLock>>>;

pub type FileLock = std::sync::Arc<tokio::sync::Mutex<()>>;

// The distinct paths a run has changed — a set, not a count: nothing here
// asks how many times.
#[derive(Default)]
struct Written {
    paths: std::collections::BTreeSet<std::path::PathBuf>,
}

pub type Viewed =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, String>>>;

impl Ctx {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            cancel: tokio_util::sync::CancellationToken::new(),
            file_locks: Default::default(),
            viewed: Default::default(),
            writes: Default::default(),
            session: None,
            spill_root: spill::temp(),
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

    /// Name the session, and the durable `spill_root` its spills are filed
    /// under by that name. Every session shares one root — a subagent's spills
    /// sit under its own name in the same tree, so the parent can read them
    /// back — and a resumed session keeps its id so new spills rejoin the old.
    pub fn with_session(
        mut self,
        id: impl Into<String>,
        spill_root: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.session = Some(state::file_stem(&id.into()));
        self.spill_root = spill_root.into();
        self
    }

    /// Where spills land, overriding the session default. Tests and alternate
    /// hosts point this at a directory of their own instead of the user's.
    pub fn with_spill_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.spill_root = root.into();
        self
    }

    /// Where over-long outputs land. A child context keeps its parent's: the
    /// two have to agree, or a `spill:<ns>/<n>` the child prints names a file
    /// its caller cannot resolve.
    pub fn spill_root(&self) -> &std::path::Path {
        &self.spill_root
    }

    /// The session this context runs in, when it has one.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub fn spill_namespace(&self) -> &str {
        self.session.as_deref().unwrap_or("default")
    }

    /// Resolve a `spill:` locator to the file it names. The workspace gate is
    /// deliberately not applied: spill files live outside the workspace, and
    /// only locators of the shape our own writer mints are accepted.
    pub fn spill_path(&self, locator: &str) -> Result<std::path::PathBuf, ToolError> {
        spill::locate(&self.spill_root, locator)
    }

    /// Record the content hash a view of `path` was built on. Feeds the
    /// staleness note beside an edit's report and the read-before-edit rule.
    pub fn note_view(&self, path: &std::path::Path, hash: &str) {
        self.viewed
            .lock()
            .expect("viewed poisoned")
            .insert(path.to_path_buf(), hash.to_string());
    }

    /// The content hash of the last view of `path`, when there was one.
    pub fn viewed_hash(&self, path: &std::path::Path) -> Option<String> {
        self.viewed
            .lock()
            .expect("viewed poisoned")
            .get(path)
            .cloned()
    }

    /// Record that this run wrote `path`. Called by the tools that change the
    /// tree, so what a run says it did can be read against what it did.
    pub fn note_write(&self, path: &std::path::Path) {
        let mut written = self.writes.lock().expect("writes poisoned");
        written.paths.insert(path.to_path_buf());
    }

    /// Every path this run has written, in sorted order.
    pub fn writes(&self) -> Vec<std::path::PathBuf> {
        self.writes
            .lock()
            .expect("writes poisoned")
            .paths
            .iter()
            .cloned()
            .collect()
    }

    /// Start a record of this run's own writes, leaving the one it was cloned
    /// from alone. What a caller wants of a subagent is what the *child*
    /// wrote; a shared record answers with both and names neither.
    pub fn with_own_writes(mut self) -> Self {
        self.writes = Default::default();
        self
    }

    /// Hold this while mutating `path`. Keyed on the resolved path, not the
    /// inode: a hard link is cut by the rename every write does (see `write`).
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
