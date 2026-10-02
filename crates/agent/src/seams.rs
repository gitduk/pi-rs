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
use tool::Tier;

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
pub trait Archive: Send + Sync {
    // No screen, so the transcript is the only record. Called once with the
    // whole transcript, finished or cut short; `parent` is the calling session.
    fn keep(&self, parent: &str, id: &str, session: Session);
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
/// Handed to the compactor rather than kept, because a summary is a model
/// call too — a spec held from startup would send it to the endpoint and key
/// a mid-session model switch already left behind.
#[derive(Clone, Copy)]
pub struct Working<'a> {
    pub transport: &'a dyn Transport,
    pub spec: &'a ModelSpec,
}

/// What a transcript is shrunk by when it outgrows the window.
///
/// The loop measures and asks; the implementation decides what goes. Every
/// method defaults to touching nothing — a real contract, not a convenience,
/// since the loop only ever needs the budget met — so [`Untouched`] qualifies.
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
/// Shared, not a channel: the surface still needs to see what hasn't been
/// taken yet (status line, hand-back at run end). Read once a turn, at the
/// one seam a user message can follow results without stranding a `tool_use`.
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

    // Push and take on a `Vec` can't panic, so this lock can't actually be
    // poisoned; recovering rather than unwrapping keeps one bad call local.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
