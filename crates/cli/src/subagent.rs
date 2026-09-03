//! The surface's side of a subagent's return line.
//!
//! A subagent has no lane, no view and no place on the bar, so these two are
//! the whole of what it leaves behind. Invisible while it runs is a thing we
//! accepted; uncounted is not.

use std::sync::{Arc, Mutex};

use agent::Totals;
use agent::session::Session;
use agent::task::Home;

use crate::session::Store;

pub struct Filed {
    store: Store,
    /// The checkout it ran in — the same one its caller is in, because a
    /// subagent does not get a tree of its own.
    root: std::path::PathBuf,
    model: String,
    /// Drained by the surface into the run's own figures. Shared rather than
    /// returned, because the tool that fills it answers to `Arc<dyn Tool>` and
    /// there is nothing to return it through.
    spent: Arc<Mutex<Totals>>,
}

impl Filed {
    /// Handed straight out as the trait object the tool takes: nothing here
    /// needs the concrete type, and a caller that has to spell the coercion is
    /// a caller doing this crate's job.
    pub fn armed(
        store: Store,
        root: std::path::PathBuf,
        model: String,
        spent: Arc<Mutex<Totals>>,
    ) -> Arc<dyn Home> {
        Arc::new(Self {
            store,
            root,
            model,
            spent,
        })
    }
}

impl Home for Filed {
    /// Named rather than left blank: `/resume`'s listing offers sessions to go
    /// back to, and this is not one — it is a record of something that already
    /// happened inside somebody else's turn.
    fn keep(&self, id: &str, session: &Session) {
        if let Err(e) = self.store.save(
            id,
            &self.root,
            &self.model,
            Some("subagent"),
            crate::session::now(),
            session,
        ) {
            tracing::warn!(
                target: "pi::session",
                id,
                error = %format!("{e:#}"),
                "subagent transcript not saved"
            );
        }
    }

    fn spent(&self, totals: &Totals) {
        if let Ok(mut held) = self.spent.lock() {
            held.merge(totals);
        }
    }
}

/// Take what subagents have spent, and leave the tally empty.
///
/// Taken, not read, and in one place because more than one surface folds it in:
/// a figure read twice is a figure counted twice.
pub fn drain(spent: &Mutex<Totals>) -> Totals {
    spent
        .lock()
        .map(|mut held| std::mem::take(&mut *held))
        .unwrap_or_default()
}
