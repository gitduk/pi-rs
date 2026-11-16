//! Child conversation checkpoints and final saves in independent session files.
//!
//! Child usage returns through the tool result to its parent's meter.

use std::sync::Arc;

use agent::session::Session;
use agent::{Archive, ArchiveWriter};
use pi_store::session::{Location, Store, now};

pub struct Filed {
    store: Store,
    root: std::path::PathBuf,
}

impl Filed {
    pub fn armed(store: Store, root: std::path::PathBuf) -> Arc<dyn Archive> {
        Arc::new(Self { store, root })
    }
}

struct Writer {
    store: Store,
    root: std::path::PathBuf,
    at: Location,
    parent: String,
    model: String,
    created: u64,
}

impl Writer {
    fn save(&self, session: Session) -> futures::future::BoxFuture<'_, Result<(), String>> {
        // The queued job also owns the index if its awaiting run is cancelled.
        let (store, root, at, parent, model, created) = (
            self.store.clone(),
            self.root.clone(),
            self.at.clone(),
            self.parent.clone(),
            self.model.clone(),
            self.created,
        );
        Box::pin(async move {
            super::saves::wait(move || {
                store.save_subagent_at(&at, &parent, &root, &model, created, &session)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("child transcript not saved: {e:#}"))
        })
    }
}

impl Archive for Filed {
    fn open(&self, parent: &str, id: &str, model: &str) -> Box<dyn ArchiveWriter + '_> {
        Box::new(Writer {
            store: self.store.clone(),
            root: self.root.clone(),
            at: self.store.path_of(&self.root, id),
            parent: parent.into(),
            model: model.into(),
            created: now(),
        })
    }
}

impl ArchiveWriter for Writer {
    fn checkpoint(&self, session: &Session) -> futures::future::BoxFuture<'_, Result<(), String>> {
        self.save(session.clone())
    }

    fn finish(&self, session: Session) -> futures::future::BoxFuture<'_, Result<(), String>> {
        self.save(session)
    }
}

/// A run with nowhere to file keeps no child transcripts either.
pub fn nowhere() -> Arc<dyn Archive> {
    Arc::new(Nowhere)
}

struct Nowhere;

impl Archive for Nowhere {
    fn open(&self, _parent: &str, _id: &str, _model: &str) -> Box<dyn ArchiveWriter + '_> {
        Box::new(Nowhere)
    }
}

impl ArchiveWriter for Nowhere {
    fn finish(&self, _session: Session) -> futures::future::BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}
