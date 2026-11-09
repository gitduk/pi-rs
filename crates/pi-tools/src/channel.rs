//! Session messages carried alongside command execution.

#[cfg(unix)]
pub(crate) mod process;

use crate::background::Scope;
use tokio::sync::oneshot;
mod events;
pub mod wire;
pub use events::{Receiver, Sender, channel};

/// Where a call still in its turn puts a question to the person.
pub trait Ask: Send + Sync {
    /// Present a registered model input, distinct from a capability's internal prompt.
    fn user_input(
        &self,
        scope: Scope,
        _request: &message::RequestId,
        asker: &str,
        asked: Asked,
    ) -> Result<Option<Answer>, String> {
        self.ask(scope, asker, asked)
    }

    /// Read the accepted model-directed answer before projecting capability output.
    fn response(
        &self,
        _request: &message::RequestId,
    ) -> Result<Option<std::sync::Arc<message::Message>>, String> {
        Ok(None)
    }

    /// Put `asked` from `asker` in `scope`. With `back` the pick comes back on
    /// the receiver — `None` when they closed it or it was withdrawn — and is
    /// not sent on; without, it goes out as if typed and nothing comes back.
    fn ask(&self, scope: Scope, asker: &str, asked: Asked) -> Result<Option<Answer>, String>;
}

/// Asks no one: a host with nowhere to put a question says so.
pub struct NoOne;

impl Ask for NoOne {
    fn ask(&self, _scope: Scope, _asker: &str, _asked: Asked) -> Result<Option<Answer>, String> {
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

/// Acceptance is separate from the request's eventual semantic response.
#[derive(Debug, Clone)]
pub struct Receipt {
    pub request: message::RequestId,
    pub cursor: Cursor,
    pub response: Option<std::sync::Arc<message::Message>>,
}

/// The next semantic log position to read in one conversation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub session: message::SessionId,
    pub next: usize,
}

/// Observation metadata is separate from the unchanged semantic message.
#[derive(Debug, Clone)]
pub struct Observed {
    pub cursor: Cursor,
    pub message: std::sync::Arc<message::Message>,
}

/// Pull-based observation does not execute messages or own their cancellation.
#[async_trait::async_trait]
pub trait Observation: Send {
    fn cursor(&self) -> &Cursor;
    async fn next(&mut self) -> Result<Observed, String>;
}

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
    Question {
        id: u64,
        asked: Asked,
        request: Option<message::MessageId>,
    },
    Linked {
        operation: u64,
        request: message::MessageId,
    },
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
    fn listen(&self, to: Sender);
    /// Admit an explicit model input without parsing its text as host commands.
    fn message(&self, message: message::WireMessage) -> Result<Receipt, String>;
    fn observe(
        &self,
        session: message::SessionId,
        cursor: Option<Cursor>,
    ) -> Result<Box<dyn Observation>, String>;
    fn watch_prompts(&self);
    fn answer_text(&self, question: u64, text: String);
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

impl Told {
    fn retained_bytes(&self) -> usize {
        let extra = match self {
            Self::Response { event, .. } => match event {
                Response::Queued | Response::Started => 0,
                Response::Progress(text) => text.capacity(),
                Response::Finished { text, outcome } => {
                    text.capacity().saturating_add(match outcome {
                        Outcome::Done | Outcome::Exited(_) => 0,
                        Outcome::Stopped(text) | Outcome::Failed(text) => text.capacity(),
                    })
                }
            },
            Self::Progress(text) | Self::Reply(text) => text.capacity(),
            Self::Answer(Ok(Some(text))) | Self::Answer(Err(text)) => text.capacity(),
            Self::Question { asked, .. } => asked
                .question
                .capacity()
                .saturating_add(asked.options.iter().map(String::capacity).sum::<usize>())
                .saturating_add(asked.options.capacity() * std::mem::size_of::<String>()),
            Self::Refused { why, .. } => why.capacity(),
            Self::Started
            | Self::Asked
            | Self::Answer(Ok(None))
            | Self::Closed(_)
            | Self::Linked { .. } => 0,
        };
        extra.saturating_add(1024)
    }
}
