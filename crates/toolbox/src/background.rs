//! What a tool needs from its host to leave its turn: somewhere to keep the
//! call as a job, and a line back for what it reports. The host implements
//! these; a tool that can go to the background is built holding a `JobSink`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
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
pub trait JobSink: Send + Sync {
    fn start(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
        told: Option<UnboundedSender<Told>>,
    ) -> Arc<dyn JobHandle>;
}

/// A running job's way to report. Each `result` comes back to its checkout
/// as a turn of its own, with what it `spent` getting there; an `input` is a
/// person speaking through the job, read as if typed; `end` says it will
/// report no more.
pub trait JobHandle: Send + Sync {
    fn id(&self) -> u64;
    fn status(&self, text: String);
    fn result(&self, text: String, spent: llm::stream::Usage);
    fn input(&self, text: String);
    /// A line for the person, never the model: a code to scan, say.
    fn notice(&self, text: String);
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
}
