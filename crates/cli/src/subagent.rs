//! The surface's side of a subagent's return line.
//!
//! A subagent has no lane, no view and no place on the bar, so this is the
//! whole of what it leaves behind. What it spent no longer lands here: the
//! tool result carries it back, and the run that called it reports it.

use std::sync::Arc;
use std::sync::OnceLock;

use agent::session::Session;
use agent::task::Home;

use crate::session::Store;

/// The saves a subagent handed off to a background thread, still in flight.
/// The exit path drains these — a transcript promised on disk has to be there
/// when the process goes, or the handoff was just a faster way to lose it.
fn pending() -> &'static std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> {
    static PENDING: OnceLock<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        OnceLock::new();
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
    /// The checkout it ran in — the same one its caller is in, because a
    /// subagent does not get a tree of its own.
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

impl Home for Filed {
    /// Named rather than left blank: `/resume`'s listing offers sessions to go
    /// back to, and this is not one — it is a record of something that already
    /// happened inside somebody else's turn.
    ///
    /// The save runs on a blocking thread so several parallel subagents do not
    /// each serialize megabytes on the tool path; the handle is registered so
    /// [`flush`] can wait for it before the process goes.
    fn keep(&self, id: &str, session: Session) {
        let (store, root, model, id) = (self.store.clone(), self.root.clone(), self.model.clone(), id.to_string());
        let handle = tokio::task::spawn_blocking(move || {
            let saved = store.save(
                &id,
                &root,
                &model,
                Some("subagent"),
                crate::session::now(),
                &session,
            );
            if let Err(e) = saved {
                tracing::warn!(
                    target: "pi::session",
                    id,
                    error = %format!("{e:#}"),
                    "subagent transcript not saved"
                );
            }
        });
        pending().lock().unwrap().push(handle);
    }
}
