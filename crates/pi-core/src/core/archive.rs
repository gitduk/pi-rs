//! Child conversation checkpoints and final saves in the workspace database.
//!
//! Child usage returns through the tool result to its parent's meter.

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

    fn write_job(
        &self,
        parent: &str,
        id: &str,
        model: &str,
        session: Session,
    ) -> impl FnOnce() -> anyhow::Result<pi_store::session::Location> + Send + 'static {
        let (store, root, parent, id, model) = (
            self.store.clone(),
            self.root.clone(),
            parent.to_owned(),
            id.to_owned(),
            model.to_owned(),
        );
        move || store.save_subagent(&parent, &id, &root, &model, now(), &session)
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
    fn draft<'a>(
        &'a self,
        _parent: &'a str,
        id: &'a str,
        draft: agent::session::Draft,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        let at = self.store.path_of(&self.root, id);
        Box::pin(async move {
            super::saves::wait(move || pi_store::session::append_draft(&at, &draft))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("child draft save failed: {e:#}"))
        })
    }

    fn checkpoint<'a>(
        &'a self,
        parent: &'a str,
        id: &'a str,
        model: &'a str,
        session: &Session,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        let write = self.write_job(parent, id, model, session.clone());
        Box::pin(async move {
            super::saves::wait(write)
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("child checkpoint failed: {e:#}"))?;
            Ok(())
        })
    }

    // On the saver thread: several subagents saving must not each serialize
    // megabytes on the tool path, and `flush` joins whatever is queued.
    fn keep(&self, parent: &str, id: &str, session: Session) {
        self.keep_model(parent, id, &self.model, session);
    }

    fn keep_model(&self, parent: &str, id: &str, model: &str, session: Session) {
        let write = self.write_job(parent, id, model, session);
        let id = id.to_owned();
        super::saves::behind(move || {
            if let Err(e) = write() {
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
