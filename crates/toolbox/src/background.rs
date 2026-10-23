//! What a tool needs from its host to leave its turn: somewhere to keep the
//! call as a job, and a line back for what it reports. The host implements
//! these; a tool that can go to the background is built holding a `JobSink`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Reads the latest of what a running job has printed, on demand.
pub type Peek = Arc<dyn Fn() -> String + Send + Sync>;

/// Starts a job over: runs it again, reporting through the handle it is
/// given and wound down by the token, as the first run was.
pub type Rerun =
    Arc<dyn Fn(Arc<dyn JobHandle>, CancellationToken) -> Result<(), tool::ToolError> + Send + Sync>;

/// Where a call that will outlive its turn is kept: a job of the checkout at
/// `root`, which the host lists, stops through `stop`, and delivers from.
/// What the host has to tell the job goes to `told`, when it listens.
pub trait JobSink: Ask {
    fn start(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
        told: Option<UnboundedSender<Told>>,
    ) -> Arc<dyn JobHandle>;
}

/// Where a call still in its turn puts a question to the person.
pub trait Ask: Send + Sync {
    /// Put `asked` from `asker` at `root`. With `back` the pick comes back on
    /// the receiver — `None` when they closed it or it was withdrawn — and is
    /// not sent on; without, it goes out as if typed and nothing comes back.
    fn ask(&self, root: PathBuf, asker: &str, asked: Asked) -> Result<Option<Answer>, String>;
}

/// Asks no one: a host with nowhere to put a question says so.
pub struct NoOne;

impl Ask for NoOne {
    fn ask(&self, _root: PathBuf, _asker: &str, _asked: Asked) -> Result<Option<Answer>, String> {
        Err("nothing here can put a question to the person".into())
    }
}

/// A question as a tool asks it: unchecked, which the host does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
    pub back: bool,
}

/// Where the answer to a question asked with `back` arrives.
pub type Answer = oneshot::Receiver<Option<String>>;

/// A running job's way to report. Each `result` comes back to its checkout
/// as a turn of its own; an `input` is a
/// person speaking through the job, read as if typed; `end` says it will
/// report no more.
pub trait JobHandle: Send + Sync {
    fn id(&self) -> u64;
    fn status(&self, text: String);
    fn result(&self, text: String);
    fn input(&self, text: String);
    /// A line for the person, never the model: a code to scan, say.
    fn notice(&self, text: String);
    /// A question for the person, each option the line that picks it, the
    /// pick going out as if typed.
    fn choose(&self, asked: Asked);
    /// Stop whatever runs where the job was started, as the user's stop would.
    fn interrupt(&self);
    /// Where the latest of what it prints can be read while it runs.
    fn watch(&self, output: Peek);
    /// From now on it is told of every turn its checkout runs, not only
    /// the ones its `input` opened.
    fn follow(&self);
    /// How it runs again once stopped or ended; one that never says is
    /// gone then, and cannot be started over.
    fn rerun(&self, _how: Rerun) {}
    fn end(&self);
}

/// What the host tells a job about the turns its `input` opened, or every
/// turn once it follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Told {
    /// A turn one of its inputs opened has begun.
    Started,
    /// That turn has ended: its whole answer, how it ended if not cleanly,
    /// and empty when it said nothing.
    Reply(String),
    /// A question it asked without `back` is up: the pick goes out as typed.
    Asked,
    /// The person's answer to a question it asked with `back`: the pick, or
    /// none; or, either way, why the question could not be put.
    Answer(Result<Option<String>, String>),
}
