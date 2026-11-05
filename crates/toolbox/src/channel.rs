//! Session messages carried alongside command execution.

#[cfg(unix)]
pub(crate) mod process;

use std::path::PathBuf;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

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
    pub back: bool,
}

/// Where the answer to a question asked with `back` arrives.
pub type Answer = oneshot::Receiver<Option<String>>;

/// What the host tells an execution about submitted operations, or every model
/// turn once it follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Told {
    /// An operation's lifecycle, independent of other operations in the session.
    Response { request: u64, event: Response },
    /// A turn one of its inputs opened has begun.
    Started,
    /// A completed part of the answer, or progress while work continues.
    Progress(String),
    /// That turn has ended: its remaining answer, how it ended if not cleanly,
    /// excluding parts already sent as progress; empty when nothing remains.
    Reply(String),
    /// A question it asked without `back` is up: the pick goes out as typed.
    Asked,
    /// The person's answer to a question it asked with `back`: the pick, or
    /// none; or, either way, why the question could not be put.
    Answer(Result<Option<String>, String>),
    /// A question is up on its checkout, for an execution that follows to put to
    /// the person too; it answers by `id`.
    Question { id: u64, asked: Asked },
    /// Question `id` no longer stands: answered anywhere, or withdrawn.
    Closed(u64),
    /// Its answer to `question` was not taken, and why: the person, who
    /// gave it, is owed the reason.
    Refused { question: u64, why: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Queued,
    Started,
    Progress(String),
    /// Remaining content without completion notices or error details.
    Finished {
        text: String,
        outcome: Outcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The cancellation reason, empty when none was supplied.
    Stopped(String),
    Failed(String),
    Exited(i32),
}

/// Messages from one running command into its session.
pub trait Channel: Send + Sync {
    fn listen(&self, to: UnboundedSender<Told>);
    fn status(&self, text: String);
    fn result(&self, text: String);
    /// A line to submit, and what the model should know about it first.
    fn input(&self, text: String, note: String);
    fn choose(&self, asked: Asked);
    fn answer(&self, question: u64, picks: Vec<usize>);
    fn interrupt(&self);
    fn follow(&self);
    /// Claim or release the checkout's turns: while claimed, a turn that ends
    /// is not the last one, since this execution owes it another.
    fn drive(&self, on: bool);
}
