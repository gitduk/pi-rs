//! What the loop reaches the outside world through: the transport it sends on,
//! the gate it asks before a call, where a finished subagent's work goes, and
//! the lines said to a run already working.

pub use brain::transport::Transport;

use crate::session::Session;
use serde_json::Value;
use std::sync::{Arc, Mutex};
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
