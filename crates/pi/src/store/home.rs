//! Where a subagent's transcript is filed: pi's side of `agent::Home`.
//!
//! A subagent has no lane, no view and no place on the bar, so this is the
//! whole of what it leaves behind. What it spent no longer lands here: the
//! tool result carries it back, and the run that called it reports it.

use std::sync::Arc;
use std::sync::OnceLock;

use agent::Home;
use agent::session::Session;

use crate::store::session::{Store, now};

// The saves a subagent handed off to a background thread, still in flight.
// The exit path drains these — a transcript promised on disk has to be there
// when the process goes, or the handoff was just a faster way to lose it.
fn pending() -> &'static std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> {
    static PENDING: OnceLock<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> = OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Wait for every handed-off save to land. Cheap at the exit: a save already
/// finished joins instantly, and one still running is exactly the one leaving
/// would have dropped.
pub(crate) async fn flush() {
    let saves = std::mem::take(&mut *pending().lock().unwrap());
    for save in saves {
        let _ = save.await;
    }
}

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
    /// a caller doing this crate's job.
    pub fn armed(store: Store, root: std::path::PathBuf, model: String) -> Arc<dyn Home> {
        Arc::new(Self { store, root, model })
    }
}

/// A run with nowhere to file: a one-shot keeps no transcript, so the subagents
/// it calls keep none either.
pub fn nowhere() -> Arc<dyn Home> {
    Arc::new(Nowhere)
}

struct Nowhere;

impl Home for Nowhere {
    fn keep(&self, _parent: &str, _id: &str, _session: Session) {}
}

impl Home for Filed {
    // The save runs on a blocking thread so several parallel subagents do not
    // each serialize megabytes on the tool path; the handle is registered so
    // [`flush`] can wait for it before the process goes.
    fn keep(&self, parent: &str, id: &str, session: Session) {
        let (store, root, model, parent, id) = (
            self.store.clone(),
            self.root.clone(),
            self.model.clone(),
            parent.to_string(),
            id.to_string(),
        );
        let handle = tokio::task::spawn_blocking(move || {
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
        let mut pending = pending().lock().unwrap();
        // Finished handles are zombies otherwise; the vec only ever grew.
        pending.retain(|h| !h.is_finished());
        pending.push(handle);
    }
}
