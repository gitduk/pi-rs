//! Where a subagent's transcript is filed: pi's side of `agent::Archive`.
//!
//! A subagent has no lane and nothing on screen, so this is the
//! whole of what it leaves behind. What it spent no longer lands here: the
//! tool result carries it back, and the run that called it reports it.

use std::sync::Arc;

use agent::Archive;
use agent::session::Session;

use pi_store::session::{Store, now};

pub struct Filed {
    store: Store,
    // The checkout it ran in — the same one its caller is in, because a
    // subagent does not get a tree of its own.
    root: std::path::PathBuf,
    model: String,
}

impl Filed {
    /// Handed straight out as the trait object the tool takes: nothing here
    /// needs the concrete type, and a caller that has to spell the coercion is
    /// a caller doing this crate's execution.
    pub fn armed(store: Store, root: std::path::PathBuf, model: String) -> Arc<dyn Archive> {
        Arc::new(Self { store, root, model })
    }
}

/// A run with nowhere to file: a one-shot keeps no transcript, so the subagents
/// it calls keep none either.
pub fn nowhere() -> Arc<dyn Archive> {
    Arc::new(Nowhere)
}

struct Nowhere;

impl Archive for Nowhere {
    fn keep(&self, _parent: &str, _id: &str, _session: Session) {}
}

impl Archive for Filed {
    // On the saver thread: several subagents saving must not each serialize
    // megabytes on the tool path, and `flush` joins whatever is queued.
    fn keep(&self, parent: &str, id: &str, session: Session) {
        let (store, root, model, parent, id) = (
            self.store.clone(),
            self.root.clone(),
            self.model.clone(),
            parent.to_string(),
            id.to_string(),
        );
        super::saves::behind(move || {
            let saved = store.save_subagent(&parent, &id, &root, &model, now(), &session);
            if let Err(e) = saved {
                tracing::warn!(
                    target: "pi::session",
                    id,
                    error = %format!("{e:#}"),
                    "subagent transcript not saved"
                );
            }
        });
    }
}
