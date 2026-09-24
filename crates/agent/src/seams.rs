//! What the loop reaches the outside world through: the transport it sends on,
//! the gate it asks before a call, where a finished subagent's work goes, what
//! shrinks a transcript that outgrows the window, and the lines said to a run
//! already working.

pub use llm::transport::Transport;

use crate::Report;
use crate::event::Event;
use crate::session::Session;
use async_trait::async_trait;
use llm::message::Message;
use llm::model::ModelSpec;
use llm::stream::Usage;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tools::Tier;

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    // The model reads this and can pick another route; a denial is a result,
    // not the end of the turn.
    Deny(String),
}

/// Gate consulted before every call. Implementations may prompt, consult a
/// policy file, or decide statically.
pub trait Approver: Send + Sync {
    fn approve(&self, name: &str, tier: Tier, args: &Value) -> Decision;
}

/// Where a finished subagent's work goes.
///
/// This layer says what it needs and the surface provides it; what a child
/// spent travels back on the tool result instead.
pub trait Home: Send + Sync {
    // A subagent has no screen, so its transcript is the only account of what
    // it did. Called once with the whole transcript, whether the run finished
    // or was cut short.
    fn keep(&self, id: &str, session: Session);
}

/// What a pass of the compactor left: what to send next, whether the transcript
/// is not what it was, and what the summary cost.
pub struct Fitted {
    pub context: Vec<Message>,
    pub changed: bool,
    /// Tokens, not money: what they cost is the surface's arithmetic.
    pub spent: Usage,
}

/// Who pays for a turn: the model doing the work, and the wire it goes out on.
///
/// A compactor is handed it because a summary is a model call like any other —
/// written by the compactor's own summarizer when one is configured, and by
/// this pair when none is. Handed in rather than kept, because a model switched
/// mid-session has to take its summaries with it: a spec held from startup
/// would send them to the endpoint and the key the session has left behind.
#[derive(Clone, Copy)]
pub struct Working<'a> {
    pub transport: &'a dyn Transport,
    pub spec: &'a ModelSpec,
}

/// What a transcript is shrunk by when it outgrows the window.
///
/// The loop measures and asks; the implementation decides what goes and what
/// the summary says. Every method defaults to touching nothing, and that is the
/// contract rather than a convenience: a transcript nobody shrinks is still a
/// transcript, and the loop carries on with what it measured. Compaction buys
/// room for the next turn — nothing in the loop needs an entry gone — so
/// [`Untouched`] is a compactor like any other.
#[async_trait]
pub trait Compactor: Send + Sync {
    /// Fit `session` into `budget` tokens if it is over, saying on `tx` what
    /// went. `urgent` is a provider that has already refused the request:
    /// holding the working tail back is a preference, fitting is not.
    async fn compact(
        &self,
        session: &mut Session,
        run: Working<'_>,
        budget: usize,
        urgent: bool,
        tx: &UnboundedSender<Event>,
    ) -> Fitted {
        let _ = (run, budget, urgent, tx);
        Fitted {
            context: session.context(),
            changed: false,
            spent: Usage::default(),
        }
    }

    /// Shrink it now, at the user's word rather than the window's: no budget is
    /// asked of it, and `focus` is what the person wants kept.
    async fn compact_now(
        &self,
        session: &mut Session,
        run: Working<'_>,
        budget: usize,
        focus: Option<&str>,
    ) -> Option<(Report, Usage)> {
        let _ = (session, run, budget, focus);
        None
    }

    /// How much of the end the compactor leaves alone, against a window of
    /// `budget` tokens. The surface says so to whoever asked for a compaction.
    fn kept_tokens(&self, budget: usize) -> usize {
        let _ = budget;
        0
    }
}

/// A compactor with nothing to compact with: every method is the trait's own
/// default, so it is the identity by construction. It is what a run with none
/// installed — a test, a `--print` — goes on.
pub struct Untouched;

#[async_trait]
impl Compactor for Untouched {}

/// Lines said after the run began, waiting for the next point where the
/// transcript can legally take one.
///
/// Shared rather than a channel, because both ends need it: the run takes what
/// is there at each seam, and the surface can still see what has not been
/// taken — count it on the status line, hand it back when the run ends. A
/// receiver is dropped by a run that was ending anyway, and a line sent into it
/// in that instant is lost with nobody left to say so.
///
/// Nothing here waits. The run looks once a turn, at the one point where a user
/// message may follow the results without stranding a `tool_use`, so there is
/// nothing an await could bring forward.
#[derive(Clone, Default)]
pub struct Steer(Arc<Mutex<Vec<String>>>);

impl Steer {
    /// Say something to the run in flight; it is heard at the next seam.
    pub fn say(&self, text: impl Into<String>) {
        self.lock().push(text.into());
    }

    /// Everything said since the last look, in the order it was said.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lock())
    }

    /// How much has been said and not yet heard.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    // A push and a take on a `Vec`, neither of which can panic, so the lock
    // cannot in fact be poisoned. Recovering rather than unwrapping keeps a
    // later one from taking the run down with it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
