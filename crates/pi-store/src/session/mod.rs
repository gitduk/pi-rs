//! Independent session files and per-session artifacts.

mod file;
mod moving;
pub use file::{Location, Transaction};
pub mod messages;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use transcript::Session;
/// The clock this crate dates transcripts by — the session's own, so a file
/// and the entries inside it are never stamped by two.
pub use transcript::now;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::text::clip;

#[derive(Debug, Serialize, Deserialize)]
pub struct Stored {
    pub id: String,
    pub workspace: String,
    /// Which model this session ran, as the endpoint names it — one
    /// consistent name across the archive, the config and the wire.
    pub model: String,
    /// When the session began. Never rewritten on save — doing so would make
    /// it a last-touched time under a name that says otherwise.
    pub created: u64,
    /// What the user calls this session. Ids are a timestamp and a pid, which
    /// nobody recognises a week later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the session has cost so far; absent in a file older than it.
    #[serde(default)]
    pub spent: llm::Totals,
    #[serde(flatten, default)]
    pub session: Session,
}

/// What a save files beside the transcript.
#[derive(Debug, Clone, Copy)]
pub struct Header<'a> {
    pub id: &'a str,
    pub model: &'a str,
    pub name: Option<&'a str>,
    pub created: u64,
    /// Here, not with the entries: a rewind takes turns back, never the money
    /// they cost.
    pub spent: llm::Totals,
}

/// A saved session as the resume list and its completion see it: the id it is
/// named by, the name the user gave it if any, and the first thing it was
/// asked, which is what stands in for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeChoice {
    pub id: String,
    pub prompt: String,
    pub name: Option<String>,
    /// When it was last worked on, which is also what the list sorts by.
    pub touched: u64,
    pub rounds: usize,
    pub bytes: u64,
}

/// How much of a session's row a list or a completion shows.
const RESUME_WIDTH: usize = 60;

impl ResumeChoice {
    /// What a row calls this session: the name, if any, then the first
    /// question — named because a first question stops meaning much later.
    ///
    /// Here, not at each surface, so a session reads the same way everywhere.
    pub fn label(&self) -> String {
        let prompt = self.prompt.trim();
        let name = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        match name {
            // Half the room at most, so a long name cannot crowd out the
            // question and leave the row saying nothing about the session.
            Some(name) if prompt.is_empty() => clip(name, RESUME_WIDTH),
            Some(name) => clip(
                &format!("{name} — {}", clip(prompt, RESUME_WIDTH / 2)),
                RESUME_WIDTH,
            ),
            None if prompt.is_empty() => "(no question)".into(),
            None => clip(prompt, RESUME_WIDTH),
        }
    }
}

impl Stored {
    /// The transcript, with any call a crash left open closed: every
    /// loader comes through here, so no run starts on a dangling `tool_use`.
    pub fn into_session(self) -> Session {
        let mut session = self.session;
        session.close_dangling();
        session
    }
}

// A path can be absent for a morning as well as for good, so nothing
// younger than this is swept — an unmounted disk costs a delay, not a transcript.
const UNREACHED_KEEP: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// One saved session, as far as it has got.
#[derive(Debug)]
pub struct Progress {
    pub id: String,
    /// The id of its last entry.
    pub last: u64,
    /// When it began.
    pub created: u64,
    /// When it was last worked on.
    pub touched: u64,
    /// Its transcript, for `Store::read`.
    pub path: Location,
}

/// Where transcripts live. Held as a value rather than read from the
/// environment at each call, so tests need no global state to isolate.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    // Shared by every clone: the core writes through one, a reader reads
    // through another.
    version: std::sync::Arc<AtomicU64>,
    indexes: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<PathBuf, std::sync::Weak<std::sync::Mutex<file::Index>>>,
        >,
    >,
}

impl Default for Store {
    // Outside the workspace: transcripts are the agent's state, not the
    // project's, and a stray file in a repo is one the user has to clean up.
    fn default() -> Self {
        Self::new(
            super::dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("sessions"),
        )
    }
}

/// A new session id. Without the counter, two minted in one second share an id
/// and with it one archive file, where the second silently replaces the first.
pub fn new_id() -> String {
    static MINTED: AtomicU64 = AtomicU64::new(0);
    let nth = MINTED.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{nth}", now(), std::process::id())
}

/// A path as a single directory name: anything but ASCII letters, digits and
/// `_` becomes `-`. Not injective (`/a/b` and `/a-b` collide), accepted since
/// a bucket's contents are read off each transcript's own workspace field.
pub fn key_of(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// Named once: four readers look it up, and naming it differently in one
// would be a session that quietly stops being found.
const TRANSCRIPT: &str = file::FILE;
const LOCK: &str = ".lock";
const BACKGROUND: &str = "background.json";
const HISTORY: &str = "history";

/// One session held by this process; the system lets go if pi dies.
pub struct Claim {
    _file: std::fs::File,
    dir: PathBuf,
}

impl Drop for Claim {
    fn drop(&mut self) {
        discard_unsaved(&self.dir);
    }
}

// Hold the session directory `dir`, made if missing, for this process.
fn claim_dir(dir: PathBuf, id: &str) -> Result<Claim> {
    // Another claim letting go removes the workspace's directory once it is
    // empty, which can fall between making it and making `dir` in it; made
    // again, it is there to stay until `dir` is.
    let made = std::fs::create_dir_all(&dir);
    if made
        .as_ref()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        std::fs::create_dir_all(&dir)?;
    } else {
        made?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(Claim { _file: file, dir }),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("session {id} is already open, in this pi or another")
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// A session directory never saved to leaves nothing behind: its lock goes,
/// and the directory with it once nothing else is there — the workspace's
/// too, when this was the last thing in it.
fn discard_unsaved(dir: &Path) {
    if !dir.join(TRANSCRIPT).exists() {
        let _ = std::fs::remove_file(dir.join(file::WRITE_LOCK));
        let _ = std::fs::remove_file(dir.join(LOCK));
        let _ = std::fs::remove_file(dir.join(HISTORY));
        if std::fs::remove_dir(dir).is_ok()
            && let Some(workspace) = dir.parent()
        {
            let _ = std::fs::remove_dir(workspace);
        }
    }
}

/// Whether some pi holds the session in `dir`, this one included: what a
/// sweep leaves alone.
pub(crate) fn held(dir: &Path) -> bool {
    std::fs::File::open(dir.join(LOCK)).is_ok_and(|f| f.try_lock().is_err())
}

// Each session owns one data file; other bucket entries are diagnostics.
fn bucket_locations(bucket: &Path) -> Vec<Location> {
    std::fs::read_dir(bucket)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path().join(TRANSCRIPT);
            file::location(path).ok().flatten()
        })
        .collect()
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            version: Default::default(),
            indexes: Default::default(),
        }
    }

    // The directory one workspace's transcripts live in.
    fn dir_of(&self, workspace: &Path) -> PathBuf {
        self.root.join(key_of(workspace))
    }

    /// The directory holding this session and its diagnostic files.
    pub fn session_dir(&self, workspace: &Path, id: &str) -> PathBuf {
        self.dir_of(workspace).join(tool::state::file_stem(id))
    }

    /// What this session's row last showed, if anything ever did.
    pub fn shown(&self, workspace: &Path, id: &str) -> Option<crate::statusline::Shown> {
        crate::statusline::Shown::read(&self.session_dir(workspace, id))
    }

    /// What `k` recalls in this session, beside its transcript: recall is
    /// what was said here, and another session in the same checkout is not.
    pub fn history_path(&self, workspace: &Path, id: &str) -> PathBuf {
        self.session_dir(workspace, id).join(HISTORY)
    }

    /// Forget the recall a checkout once kept for all its sessions: recall is
    /// filed per session now. Delete this once 2026-12 has passed.
    pub fn drop_stale_history(&self) {
        if let Some(old) = crate::dir().map(|dir| dir.join(HISTORY)) {
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::remove_dir_all(&old);
        }
        for bucket in self.buckets() {
            let _ = std::fs::remove_file(bucket.join(HISTORY));
        }
    }

    /// Where a session's background commands wait for the pi that opens it
    /// next, beside its transcript, so they go when the session does.
    pub fn background_path(&self, workspace: &Path, id: &str) -> PathBuf {
        self.session_dir(workspace, id).join(BACKGROUND)
    }

    /// Moves with every write that can change what [`Store::choices`]
    /// answers, so a cached list can tell it is behind without being told.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    fn changed(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// The tree every bucket sits in, for the sweeps that walk all of them.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn buckets(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
    }

    /// Address one session's append-only file.
    pub fn path_of(&self, workspace: &Path, id: &str) -> Location {
        let path = self.session_dir(workspace, id).join(TRANSCRIPT);
        let session = message::SessionId::new(id.to_owned()).expect("session identifier");
        let mut indexes = self
            .indexes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = indexes.get(&path).and_then(std::sync::Weak::upgrade) {
            return Location {
                path,
                session,
                index,
            };
        }
        indexes.retain(|_, index| index.strong_count() > 0);
        let at = Location::new(path, session);
        indexes.insert(at.path.clone(), std::sync::Arc::downgrade(&at.index));
        at
    }

    pub fn filed(&self, workspace: &Path, id: &str) -> bool {
        self.path_of(workspace, id).exists()
    }

    pub fn claim(&self, workspace: &Path, id: &str) -> Result<Claim> {
        claim_dir(self.session_dir(workspace, id), id)
    }

    pub fn take(&self, workspace: &Path, id: &str) -> Result<(Stored, Claim)> {
        let claim = self.relocate(workspace, id)?;
        Ok((Self::read(&self.path_of(workspace, id))?, claim))
    }

    pub fn relocate(&self, workspace: &Path, id: &str) -> Result<Claim> {
        let here = self.claim(workspace, id)?;
        let destination = self.path_of(workspace, id);
        if self.resume_move(workspace, id)? {
            self.changed();
            return Ok(here);
        }
        if destination.recorded()? {
            let (state, _) = file::tail(&destination)?.context("session is not recorded")?;
            if state
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.workspace != workspace.to_string_lossy())
            {
                let metadata = state
                    .metadata
                    .as_ref()
                    .context("missing session metadata")?;
                let original = self.path_of(Path::new(&metadata.workspace), id);
                anyhow::ensure!(
                    original == destination || !original.path.exists(),
                    "destination session conflicts with its source; refusing an incomplete copy"
                );
                self.move_family(&destination, workspace)?;
                self.changed();
            }
            return Ok(here);
        }
        for bucket in self.buckets() {
            for source in bucket_locations(&bucket) {
                if source.session.as_str() != id {
                    continue;
                }
                let Some((state, _)) = file::tail(&source)? else {
                    continue;
                };
                let Some(metadata) = state.metadata else {
                    continue;
                };
                if metadata.parent.is_some() {
                    continue;
                }
                self.move_family(&source, workspace)?;
                self.changed();
                return Ok(here);
            }
        }
        anyhow::bail!("no session `{id}` in {}", self.root.display())
    }

    pub fn save(
        &self,
        workspace: &Path,
        header: Header<'_>,
        session: &Session,
    ) -> Result<Location> {
        self.save_with(workspace, header, session, |_, _| Ok(()))
    }

    /// Conversation and request state become visible in the same commit.
    pub fn save_with(
        &self,
        workspace: &Path,
        header: Header<'_>,
        session: &Session,
        extra: impl FnOnce(&mut Transaction, &Location) -> Result<()>,
    ) -> Result<Location> {
        let at = self.path_of(workspace, header.id);
        file::update(&at, |tx| {
            file::write(tx, &at, workspace, header, session, None)?;
            extra(tx, &at)
        })?;
        self.changed();
        Ok(at)
    }

    pub fn save_subagent(
        &self,
        parent: &str,
        id: &str,
        workspace: &Path,
        model: &str,
        created: u64,
        session: &Session,
    ) -> Result<Location> {
        let at = self.path_of(workspace, id);
        self.save_subagent_at(&at, parent, workspace, model, created, session)?;
        Ok(at)
    }

    /// Keep this location alive across checkpoints to retain its incremental index.
    pub fn save_subagent_at(
        &self,
        at: &Location,
        parent: &str,
        workspace: &Path,
        model: &str,
        created: u64,
        session: &Session,
    ) -> Result<()> {
        file::update(at, |tx| {
            file::write(
                tx,
                at,
                workspace,
                Header {
                    id: at.session.as_str(),
                    model,
                    name: Some("subagent"),
                    created,
                    spent: Default::default(),
                },
                session,
                Some(parent),
            )
        })
    }

    pub fn progress(&self) -> Vec<Progress> {
        self.locations()
            .into_iter()
            .filter_map(|at| {
                let (state, _) = file::tail(&at).ok()??;
                let metadata = state.metadata?;
                if metadata.parent.is_some() {
                    return None;
                }
                Some(Progress {
                    id: state.session.as_str().into(),
                    last: state.last.map_or(0, |id| id.0),
                    created: metadata.created,
                    touched: metadata.touched,
                    path: at,
                })
            })
            .collect()
    }

    fn locations(&self) -> Vec<Location> {
        self.buckets()
            .into_iter()
            .flat_map(|bucket| bucket_locations(&bucket))
            .collect()
    }

    pub fn read(at: &Location) -> Result<Stored> {
        file::read(at)
    }

    #[cfg(test)]
    pub(crate) fn load(&self, id: &str) -> Result<Stored> {
        for at in self.locations() {
            if at.session.as_str() == id && at.recorded()? {
                return Self::read(&at);
            }
        }
        anyhow::bail!("no session `{id}` in {}", self.root.display())
    }

    pub fn remove(&self, workspace: &Path, id: &str) -> Result<()> {
        let bucket = self.dir_of(workspace);
        for at in bucket_locations(&bucket) {
            let child = file::tail(&at).ok().flatten().is_some_and(|(state, _)| {
                state
                    .metadata
                    .is_some_and(|metadata| metadata.parent.as_deref() == Some(id))
            });
            if at.session.as_str() == id || child {
                std::fs::remove_dir_all(at.artifacts())?;
            }
        }
        let dir = self.session_dir(workspace, id);
        if dir.is_dir() {
            std::fs::remove_dir_all(dir)?;
        }
        self.changed();
        Ok(())
    }

    pub fn prune(&self) {
        self.prune_older_than(UNREACHED_KEEP);
    }

    pub fn forget_older_than(&self, keep: std::time::Duration, spare: Option<&str>) {
        self.sweep(keep, |id, _| spare != Some(id));
    }

    fn prune_older_than(&self, keep: std::time::Duration) {
        self.sweep(keep, |_, metadata| !Path::new(&metadata.workspace).is_dir());
    }

    fn sweep(&self, keep: std::time::Duration, eligible: impl Fn(&str, &file::Metadata) -> bool) {
        self.changed();
        for at in self.locations() {
            let Ok(Some((state, _))) = file::tail(&at) else {
                continue;
            };
            let Some(metadata) = state.metadata else {
                continue;
            };
            if metadata.parent.is_none()
                && eligible(state.session.as_str(), &metadata)
                && !held(&at.artifacts())
                && now().saturating_sub(metadata.touched) >= keep.as_secs()
            {
                let _ = self.remove(Path::new(&metadata.workspace), state.session.as_str());
            }
        }
    }

    pub fn drop_under(&self, root: &Path) -> usize {
        self.changed();
        let mut dropped = 0;
        for at in self.locations() {
            let Ok(Some((state, _))) = file::tail(&at) else {
                continue;
            };
            let Some(metadata) = state.metadata else {
                continue;
            };
            if metadata.parent.is_none()
                && Path::new(&metadata.workspace).starts_with(root)
                && self
                    .remove(Path::new(&metadata.workspace), state.session.as_str())
                    .is_ok()
            {
                dropped += 1;
            }
        }
        dropped
    }

    pub fn choices(&self, workspace: &Path) -> Vec<ResumeChoice> {
        let mut choices: Vec<_> = bucket_locations(&self.dir_of(workspace))
            .into_iter()
            .filter_map(|at| {
                let (state, end) = file::tail(&at).ok()??;
                let metadata = state.metadata?;
                if metadata.parent.is_some() || metadata.workspace != workspace.to_string_lossy() {
                    return None;
                }
                Some(ResumeChoice {
                    id: state.session.as_str().into(),
                    prompt: metadata.prompt,
                    name: metadata.name,
                    touched: metadata.touched,
                    rounds: metadata.rounds,
                    bytes: end,
                })
            })
            .collect();
        choices.sort_by(|a, b| b.touched.cmp(&a.touched).then_with(|| b.id.cmp(&a.id)));
        choices
    }

    pub fn take_latest(&self, workspace: &Path) -> Result<(Stored, Claim)> {
        let newest = self
            .choices(workspace)
            .into_iter()
            .next()
            .with_context(|| format!("no session recorded for {}", workspace.display()))?;
        self.take(workspace, &newest.id)
    }
}

#[cfg(test)]
mod tests {
    use super::key_of;

    // One session, one holder at a time; one never saved leaves no directory.
    #[test]
    fn a_held_session_refuses_a_second_claim_until_let_go() {
        let tmp = tempfile::tempdir().unwrap();
        let store = super::Store::new(tmp.path().join("s"));
        let ws = std::path::Path::new("/w");
        let held = store.claim(ws, "a").unwrap();
        let err = store.claim(ws, "a").err().unwrap().to_string();
        assert!(err.contains("already open"), "{err}");
        drop(held);
        let dir = store.session_dir(ws, "a");
        assert!(!dir.exists());
    }

    // The workspace's directory goes with the last session in it that was
    // never saved, so a refused `take` leaves nothing; one still held stays.
    #[test]
    fn a_workspace_with_nothing_saved_leaves_no_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = super::Store::new(tmp.path().join("s"));
        let ws = std::path::Path::new("/w");
        let bucket = store.dir_of(ws);

        assert!(store.take(ws, "nope").is_err());
        assert!(!bucket.exists());

        let held = store.claim(ws, "held").unwrap();
        drop(store.claim(ws, "gone").unwrap());
        assert!(bucket.exists(), "a held session keeps it");
        drop(held);
        assert!(!bucket.exists());
    }

    #[test]
    fn two_ids_minted_in_one_second_are_still_two_ids() {
        // Same second, same pid: everything but the counter is equal.
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn an_id_cannot_walk_out_of_the_directory_it_names_a_file_in() {
        assert_eq!(
            tool::state::file_stem("../../etc/cron.d/x"),
            "______etc_cron_d_x"
        );
        assert_eq!(tool::state::file_stem(".."), "__");
    }

    use super::*;
    use llm::message::{AssistantContent, Message, ToolCall};
    use serde_json::json;

    fn header<'a>(id: &'a str, model: &'a str, name: Option<&'a str>, created: u64) -> Header<'a> {
        Header {
            id,
            model,
            name,
            created,
            spent: llm::Totals::default(),
        }
    }

    fn log_with(messages: Vec<Message>) -> Session {
        Session::from_messages(messages)
    }

    // A session saved as a log is still one a sweep of all workspaces sees:
    // reading it as the single object an earlier build wrote would drop it.
    #[test]
    fn progress_sees_a_session_saved_as_a_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let at = dir.path().join("ws");
        let mut session = Session::new();
        session.push_note("one");
        store
            .save(&at, header("s1", "m", None, 1), &session)
            .unwrap();
        // The second save makes it a log of two records.
        session.push_note("two");
        store
            .save(&at, header("s1", "m", None, 1), &session)
            .unwrap();

        let seen = store
            .progress()
            .into_iter()
            .find(|p| p.id == "s1")
            .expect("a session saved as a log");
        assert_eq!(seen.last, session.entries().last().unwrap().id().0);
    }

    // Sets what `touched()` reads. Forced through raw JSON because an
    // entry's stamp is the session's to set, not a caller's.
    fn touched_at(store: &Store, workspace: &Path, id: &str, at: u64) {
        let path = store.path_of(workspace, id);
        file::update(&path, |tx| {
            tx.state.metadata.as_mut().unwrap().touched = at;
            Ok(())
        })
        .unwrap();
    }

    fn call(name: &str) -> Message {
        Message::Assistant {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "c1".into(),
                name: name.into(),
                args: json!({}),
            })],
        }
    }

    fn results() -> Message {
        Message::tool_results(vec![llm::message::ToolResult::text("c1", "read", "body")])
    }

    // Unreachable isn't enough alone: an unmounted checkout looks like a
    // removed one. Age is what tells them apart.
    #[test]
    fn a_bucket_goes_when_its_tree_is_gone_and_not_before() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("hi")]);
        // Outside the store's own root: a workspace under it would read as one
        // more bucket, and an empty one at that.
        let live = tempfile::tempdir().unwrap();

        let save = |id: &str, at: &std::path::Path| {
            store
                .save(at, header(id, "test-model", None, 7), &log)
                .unwrap();
        };
        save("live", live.path());
        save("gone", std::path::Path::new("/no/such/checkout"));

        // Young enough that the path is not yet evidence of anything.
        store.prune_older_than(std::time::Duration::from_secs(3600));
        assert!(store.load("gone").is_ok(), "an hour is not a removed tree");

        store.prune_older_than(std::time::Duration::ZERO);
        assert!(store.load("live").is_ok(), "the tree is still there");
        assert!(
            store.load("gone").is_err(),
            "nothing can reach a bucket whose tree went"
        );
    }
    #[test]
    fn an_untouched_session_is_forgotten_but_not_the_one_being_resumed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("hi")]);
        let ws = tempfile::tempdir().unwrap();
        for id in ["old", "resumed"] {
            store
                .save(ws.path(), header(id, "test-model", None, 7), &log)
                .unwrap();
        }

        store.forget_older_than(std::time::Duration::from_secs(3600), None);
        assert!(store.load("old").is_ok(), "saved just now is not old");

        store.forget_older_than(std::time::Duration::ZERO, Some("resumed"));
        assert!(store.load("old").is_err());
        assert!(
            store.load("resumed").is_ok(),
            "the session being resumed stays"
        );
    }

    #[test]
    fn forgetting_spares_a_session_another_pi_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("hi")]);
        let ws = tempfile::tempdir().unwrap();
        let held = store.claim(ws.path(), "held").unwrap();
        store
            .save(ws.path(), header("held", "test-model", None, 7), &log)
            .unwrap();

        store.forget_older_than(std::time::Duration::ZERO, None);
        assert!(store.load("held").is_ok());
        drop(held);
        store.forget_older_than(std::time::Duration::ZERO, None);
        assert!(store.load("held").is_err());
    }

    // Two trees that fold to one bucket share it, so pruning must judge
    // each transcript by its own workspace, not the bucket as a whole.
    #[test]
    fn pruning_a_shared_bucket_spares_the_tree_that_is_still_there() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("hi")]);
        // `…/w/t` and `…/w-t` fold alike; only the first is on disk.
        let home = tempfile::tempdir().unwrap();
        let live = home.path().join("w").join("t");
        let gone = home.path().join("w-t");
        std::fs::create_dir_all(&live).unwrap();
        assert_eq!(key_of(&live), key_of(&gone), "the two trees share a bucket");
        store
            .save(&live, header("live", "test-model", None, 7), &log)
            .unwrap();
        store
            .save(&gone, header("gone", "test-model", None, 7), &log)
            .unwrap();
        assert_eq!(store.buckets().len(), 1, "one bucket, two workspaces");

        store.prune_older_than(std::time::Duration::ZERO);

        assert!(
            store.load("live").is_ok(),
            "the live tree keeps its session"
        );
        assert!(
            store.load("gone").is_err(),
            "the removed tree's session goes"
        );
    }

    // Removing a tree drops its buckets immediately rather than
    // waiting for the sweep; a sibling tree's records must survive.
    #[test]
    fn drop_under_removes_the_buckets_of_one_checkout_and_no_others() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("store"));
        let home = tempfile::tempdir().unwrap();
        let log = log_with(vec![Message::user("hi")]);
        let save = |id: &str, at: &std::path::Path| {
            store
                .save(at, header(id, "test-model", None, 7), &log)
                .unwrap();
        };
        let tree = home.path().join(".worktrees").join("feature-one");
        let deep = tree.join("crates/pi");
        let sibling = home.path().join(".worktrees").join("feature-two");
        save("in-tree", &tree);
        save("in-deep", &deep);
        save("in-sibling", &sibling);

        assert_eq!(store.drop_under(&tree), 2);
        assert!(store.load("in-tree").is_err());
        assert!(store.load("in-deep").is_err());
        assert!(store.load("in-sibling").is_ok());
    }
    #[test]
    fn drop_under_keeps_a_bucket_holding_a_transcript_it_cannot_read() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("store"));
        let tree = tempfile::tempdir().unwrap();
        store
            .save(
                tree.path(),
                header("ours", "test-model", None, 7),
                &log_with(vec![Message::user("hi")]),
            )
            .unwrap();
        let unread = store.session_dir(tree.path(), "unread").join(TRANSCRIPT);
        std::fs::create_dir_all(unread.parent().unwrap()).unwrap();
        std::fs::write(&unread, b"not json").unwrap();

        assert_eq!(store.drop_under(tree.path()), 1);
        assert!(store.load("ours").is_err());
        assert!(unread.exists());
    }
    // Two trees folding to one bucket key share it; removing one must not
    // take the other's — the recorded workspace decides, not the bucket.
    #[test]
    fn drop_under_spares_a_sibling_that_folds_to_the_same_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("store"));
        let home = tempfile::tempdir().unwrap();
        let log = log_with(vec![Message::user("hi")]);
        let tree = home.path().join(".worktrees").join("feature-x");
        let sibling = home.path().join(".worktrees").join("feature/x");
        store
            .save(&tree, header("mine", "test-model", None, 7), &log)
            .unwrap();
        store
            .save(&sibling, header("theirs", "test-model", None, 7), &log)
            .unwrap();
        assert_eq!(key_of(&tree), key_of(&sibling));

        assert_eq!(store.drop_under(&tree), 1);
        assert!(store.load("mine").is_err());
        assert!(store.load("theirs").is_ok());
    }

    #[test]
    fn a_saved_transcript_round_trips_and_stays_private() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("hi"), Message::assistant_text("there")]);
        let path = store
            .save(
                std::path::Path::new("/w"),
                header("t1", "test-model", Some("the flaky test"), 7),
                &log,
            )
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path.path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "a transcript holds prompts and file contents"
            );
        }
        assert_eq!(
            path.path,
            store.session_dir(Path::new("/w"), "t1").join(TRANSCRIPT)
        );
        let back = store.load("t1").unwrap();
        assert_eq!(back.model, "test-model");
        assert_eq!(back.name.as_deref(), Some("the flaky test"));
        assert_eq!(back.into_session(), log);

        // Listing reads session metadata without rebuilding the conversation.
        let listed = store.choices(std::path::Path::new("/w"));
        assert_eq!(listed[0].name.as_deref(), Some("the flaky test"));
    }

    #[test]
    fn a_transcript_with_tool_traffic_survives_the_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("go"), call("read"), results()]);
        store
            .save(std::path::Path::new("/w"), header("t", "m", None, 1), &log)
            .unwrap();

        let back = store.load("t").unwrap().into_session();
        assert_eq!(
            back.context().len(),
            3,
            "tool traffic must not vanish in transit"
        );
        assert_eq!(back.context()[1].tool_calls().next().unwrap().name, "read");
    }

    #[test]
    fn a_compaction_record_survives_the_round_trip_with_its_history() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let mut log = log_with(vec![Message::user("go"), call("read"), results()]);
        // The result is its own entry now, so the omission names that entry.
        let target = log.view().last().unwrap().id();
        log.record(transcript::Compaction {
            omissions: vec![transcript::Omission {
                block: None,
                entry: target,
                notice: "[gone]".into(),
            }],
            ..Default::default()
        });
        store
            .save(std::path::Path::new("/w"), header("t", "m", None, 1), &log)
            .unwrap();

        let back = store.load("t").unwrap().into_session();
        // The view is shrunk, and the body that was omitted is still on disk.
        assert!(format!("{:?}", back.context()[2]).contains("[gone]"));
        assert!(
            back.entries()
                .iter()
                .any(|e| format!("{e:?}").contains("body"))
        );
    }

    #[test]
    fn latest_picks_the_newest_session_for_that_workspace_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("x")]);
        store
            .save(
                std::path::Path::new("/a"),
                header("old", "m", None, 1),
                &log,
            )
            .unwrap();
        store
            .save(
                std::path::Path::new("/b"),
                header("other", "m", None, 1),
                &log,
            )
            .unwrap();
        // A second session in the same workspace, named `new` to sort after
        // `old` by id — so only recency can put it first.
        store
            .save(
                std::path::Path::new("/a"),
                header("new", "m", None, 1),
                &log,
            )
            .unwrap();

        touched_at(&store, std::path::Path::new("/a"), "old", 100);
        touched_at(&store, std::path::Path::new("/a"), "new", 200);

        assert_eq!(
            store.take_latest(std::path::Path::new("/a")).unwrap().0.id,
            "new"
        );
        assert_eq!(
            store.take_latest(std::path::Path::new("/b")).unwrap().0.id,
            "other"
        );
        assert!(store.take_latest(std::path::Path::new("/nowhere")).is_err());
    }

    // Deliberately not injective — `/home/u/pi-rs` and `/home/u/pi/rs`
    // share a bucket; accepted, since the bucket name is never read as the answer.
    #[test]
    fn a_workspace_key_is_its_path_with_every_separator_dashed() {
        use std::path::Path;
        assert_eq!(key_of(Path::new("/home/dev/pi-rs")), "-home-dev-pi-rs");
        assert_eq!(key_of(Path::new("/home/dev/api_v2")), "-home-dev-api_v2");
        assert_eq!(key_of(Path::new("/")), "-");
        assert_eq!(key_of(Path::new(".")), "-");
        assert_eq!(
            key_of(Path::new("/home/u/pi-rs")),
            key_of(Path::new("/home/u/pi/rs"))
        );
    }

    // Spending survives saves and metadata updates.
    #[test]
    fn spend_is_kept_with_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let w = Path::new("/w");
        let spent = llm::Totals {
            usage: llm::Usage {
                input: 1_200,
                cache_read: 30_000,
                ..Default::default()
            },
            cost: 0.25,
        };
        let log = log_with(vec![Message::user("x")]);
        let path = store
            .save(
                w,
                Header {
                    spent,
                    ..header("s", "m", None, 1)
                },
                &log,
            )
            .unwrap();
        assert_eq!(store.take(w, "s").unwrap().0.spent, spent);

        file::update(&path, |tx| {
            tx.state.metadata.as_mut().unwrap().spent = llm::Totals::default();
            Ok(())
        })
        .unwrap();
        assert_eq!(store.load("s").unwrap().spent, llm::Totals::default());
    }

    // Taken from another workspace, a session moves rather than forks: one
    // copy, under the workspace now using it — never while another pi has it.
    #[test]
    fn a_session_taken_elsewhere_moves_with_everything_beside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        let log = log_with(vec![Message::user("x")]);
        store.save(a, header("s", "m", None, 1), &log).unwrap();
        store.save_subagent("s", "s-sub", a, "m", 2, &log).unwrap();
        use std::io::Write;
        let mut artifact =
            std::fs::File::create(store.session_dir(a, "s").join("artifact.txt")).unwrap();
        writeln!(artifact, "before move").unwrap();

        let held = store.claim(a, "s").unwrap();
        let err = store.take(b, "s").err().unwrap().to_string();
        assert!(err.contains("already open"), "{err}");
        assert!(store.filed(a, "s"), "refused, nothing moved");
        drop(held);

        let (stored, _claim) = store.take(b, "s").unwrap();
        assert_eq!(stored.id, "s");
        assert!(store.filed(b, "s"));
        assert!(store.path_of(b, "s-sub").exists());
        assert!(!store.session_dir(a, "s").exists());
        writeln!(artifact, "after move").unwrap();
        assert_eq!(
            std::fs::read_to_string(store.session_dir(b, "s").join("artifact.txt")).unwrap(),
            "before move\nafter move\n"
        );
    }

    // A cached resume list is dropped by this number alone: a
    // write that changes `choices` without moving it leaves the list stale.
    #[test]
    fn a_failed_family_move_resumes_before_opening_its_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        let mut log = Session::with_prompt("first");
        store.save(a, header("s", "m", None, 1), &log).unwrap();
        log.prompt("second");
        store.save(a, header("s", "m", None, 1), &log).unwrap();
        store.save_subagent("s", "child", a, "m", 2, &log).unwrap();
        let source = store.session_dir(a, "s");
        let destination = store.session_dir(b, "s");
        std::fs::write(source.join("artifact"), "complete").unwrap();
        std::fs::create_dir_all(destination.join("artifact")).unwrap();
        // Fail after transcript publication, while moving an artifact.
        assert!(store.relocate(b, "s").is_err());
        assert!(source.join(TRANSCRIPT).exists());
        assert!(destination.join(".move.json").exists());
        // Simulate a torn target too. The intent forces a fresh source copy.
        let target = store.path_of(b, "s");
        let text = std::fs::read_to_string(&target.path).unwrap();
        std::fs::write(
            &target.path,
            text.lines().take(2).collect::<Vec<_>>().join("\n") + "\n",
        )
        .unwrap();
        std::fs::remove_dir(destination.join("artifact")).unwrap();
        let (stored, claim) = store.take(b, "s").unwrap();
        assert_eq!(stored.session.entries().len(), 2);
        assert_eq!(
            Store::read(&store.path_of(b, "child"))
                .unwrap()
                .session
                .entries()
                .len(),
            2
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("artifact")).unwrap(),
            "complete"
        );
        assert!(!source.exists());
        assert!(!store.session_dir(a, "child").exists());
        assert!(!destination.join(".move.json").exists());
        drop(claim);
        // The intent and staging files are temporary, including across another move.
        assert_eq!(store.take(a, "s").unwrap().0.session.entries().len(), 2);
    }

    #[test]
    fn writes_that_change_the_choices_move_the_version_every_clone_sees() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let reader = store.clone();
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        let log = log_with(vec![Message::user("x")]);

        let mut seen = reader.version();
        let mut moved = |what: &str| {
            assert_ne!(reader.version(), seen, "{what} left the version");
            seen = reader.version();
        };
        store
            .save(a, header("s", "m", Some("named"), 1), &log)
            .unwrap();
        moved("save");
        let (_, claim) = store.take(b, "s").unwrap();
        drop(claim);
        moved("a move from another workspace");
        store.drop_under(b);
        moved("drop_under");
        store.forget_older_than(std::time::Duration::ZERO, None);
        moved("forget_older_than");
    }

    #[test]
    fn a_subagent_has_its_own_file_and_is_never_offered_back() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let w = std::path::Path::new("/w");
        let log = log_with(vec![Message::user("x")]);
        store.save(w, header("p", "m", None, 1), &log).unwrap();
        let at = store
            .save_subagent("p", "p-subagent-0", w, "m", 2, &log)
            .unwrap();

        store
            .save_subagent("p", "p-subagent-0", w, "m", 20, &log)
            .unwrap();
        assert_eq!(Store::read(&at).unwrap().created, 2);
        assert_ne!(at.path, store.path_of(w, "p").path);
        let ids: Vec<String> = store.choices(w).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["p"]);
        assert_eq!(store.take_latest(w).unwrap().0.id, "p");
    }

    #[test]
    fn list_returns_this_workspaces_sessions_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("x")]);
        store
            .save(std::path::Path::new("/w"), header("a", "m", None, 1), &log)
            .unwrap();
        store
            .save(std::path::Path::new("/w"), header("b", "m", None, 1), &log)
            .unwrap();
        store
            .save(
                std::path::Path::new("/other"),
                header("c", "m", None, 1),
                &log,
            )
            .unwrap();

        store
            .save(std::path::Path::new("/w"), header("z", "m", None, 1), &log)
            .unwrap();

        // Expected order is the reverse of the ids' own, so a list that
        // fell back to breaking ties by id would fail, not pass by accident.
        touched_at(&store, std::path::Path::new("/w"), "a", 300);
        touched_at(&store, std::path::Path::new("/w"), "b", 200);
        touched_at(&store, std::path::Path::new("/w"), "z", 100);

        let ids: Vec<String> = store
            .choices(std::path::Path::new("/w"))
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(ids, ["a", "b", "z"]);
        // Another workspace's sessions stay out.
        assert!(
            store
                .choices(std::path::Path::new("/other"))
                .iter()
                .all(|c| c.id == "c")
        );
    }

    // A session that never got a prompt still belongs in the resume index;
    // only its lack of a name distinguishes it.
    #[test]
    fn an_empty_session_is_still_listed_to_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let asked = log_with(vec![
            Message::user("why is the flaky test flaky?"),
            Message::assistant_text("there"),
        ]);
        store
            .save(
                std::path::Path::new("/w"),
                header("a", "m", None, 1),
                &asked,
            )
            .unwrap();
        store
            .save(
                std::path::Path::new("/w"),
                header("b", "m", None, 1),
                &Session::new(),
            )
            .unwrap();

        let got = store.choices(std::path::Path::new("/w"));
        assert_eq!(got.len(), 2, "a session with no prompt is still resumable");
        assert!(got.iter().any(|c| c.id == "a" && !c.prompt.is_empty()));
        assert!(got.iter().any(|c| c.id == "b"));
    }

    #[test]
    fn sessions_are_filed_under_a_workspace_bucket_named_by_sanitized_path() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("x")]);
        store
            .save(std::path::Path::new("/w"), header("t", "m", None, 1), &log)
            .unwrap();

        // Each session owns its data file inside the workspace bucket.
        let bucket = tmp.path().join("-w").join("t").join(TRANSCRIPT);
        assert!(
            bucket.is_file(),
            "session must be filed under its workspace bucket"
        );
        assert!(
            !tmp.path().join("t.json").exists(),
            "no flat file beside the buckets"
        );

        // And it loads from the bucket by id alone.
        assert_eq!(store.load("t").unwrap().id, "t");
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;
    use llm::message::{AssistantContent, Message};
    use serde_json::json;

    fn header(id: &str) -> Header<'_> {
        Header {
            id,
            model: "m",
            name: None,
            created: 1,
            spent: Default::default(),
        }
    }

    #[test]
    fn session_files_isolate_writes_and_rewind_keeps_original_content() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("state"));
        let workspace = temp.path().join("work");
        let mut threads = Vec::new();
        for id in ["a", "b"] {
            let store = store.clone();
            let workspace = workspace.clone();
            threads.push(std::thread::spawn(move || {
                let mut session = Session::new();
                for n in 0..5 {
                    session.prompt(format!("{id}-{n}"));
                    store.save(&workspace, header(id), &session).unwrap();
                }
                session
            }));
        }
        let mut first = threads.remove(0).join().unwrap();
        let second = threads.remove(0).join().unwrap();
        let a = store.path_of(&workspace, "a");
        let b = store.path_of(&workspace, "b");
        assert_ne!(a.path, b.path);
        assert_eq!(Store::read(&a).unwrap().session, first);
        assert_eq!(Store::read(&b).unwrap().session, second);
        let before = first.entries().len();
        first.rollback_before(first.entries()[2].id());
        first.prompt("replacement");
        store.save(&workspace, header("a"), &first).unwrap();
        assert_eq!(Store::read(&a).unwrap().session, first);
        let retained: usize = std::fs::read_to_string(&a.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .flat_map(|line| match line {
                serde_json::Value::Array(records) => records,
                record => vec![record],
            })
            .filter(|record| record["entry"].is_number())
            .count();
        assert_eq!(
            retained,
            before + 1,
            "rewinding retains the original records"
        );
        store.remove(&workspace, "a").unwrap();
        assert_eq!(Store::read(&b).unwrap().session, second);
        assert!(!a.exists());
    }

    #[test]
    fn tool_turns_share_bodies_and_keep_protocol_identity_after_rewind() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("state"));
        let workspace = temp.path().join("work");
        let mut session = Session::with_prompt("tool-input-marker");
        let wire = |id: &str,
                    by: Option<&str>,
                    kind: serde_json::Value,
                    body: serde_json::Value,
                    native: Option<&str>| {
            json!({"message":{"id":id,"session":"s","by":by,"at":1,"kind":kind,"body":body},
                "parent":native.map(|_| "u1"),"native_call":native})
        };
        let user = wire(
            "u1",
            None,
            json!({"type":"request","value":{"direction":"to_model","request":"user"}}),
            json!([{"type":"text","value":"tool-input-marker"}]),
            None,
        );
        let mut protocol = vec![user];
        let save = |session: &Session, base: usize, protocol: &[serde_json::Value]| {
            store
                .save_with(&workspace, header("s"), session, |tx, at| {
                    messages::write_in(tx, at, &at.session, base, &protocol[base..])
                })
                .unwrap()
        };
        let at = save(&session, 0, &protocol);
        let args = json!({"command":"tool-arguments-marker"});
        let call = session.push_assistant(vec![
            AssistantContent::Text(llm::message::Text {
                text: "working-marker".into(),
            }),
            AssistantContent::ToolCall(llm::message::ToolCall {
                id: "c1".into(),
                name: "run".into(),
                args: args.clone(),
            }),
        ]);
        protocol.push(wire("t1", Some("u1"),
            json!({"type":"request","value":{"direction":"from_model","request":{"type":"tool","name":"run"}}}),
            json!([{"type":"data","value":args}]), Some("c1")));
        save(&session, 1, &protocol);
        assert_eq!(
            Store::read(&at).unwrap().session,
            session,
            "the call is durable before its result"
        );
        session.push_results(vec![llm::message::ToolResult::text(
            "c1",
            "run",
            "tool-result-marker",
        )]);
        protocol.push(wire(
            "tr1",
            Some("t1"),
            json!({"type":"response","value":{"request":"t1","status":{"type":"done"}}}),
            json!([{"type":"text","value":"tool-result-marker"}]),
            None,
        ));
        save(&session, 2, &protocol);
        session.push_assistant(vec![AssistantContent::Text(llm::message::Text {
            text: "done-marker".into(),
        })]);
        protocol.push(wire(
            "a1",
            Some("u1"),
            json!({"type":"response","value":{"request":"u1","status":{"type":"done"}}}),
            json!([{"type":"text","value":"working-markerdone-marker"}]),
            None,
        ));
        save(&session, 3, &protocol);
        assert_eq!(Store::read(&at).unwrap().session, session);
        let recovered = messages::read(&at).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&recovered.entries).unwrap(),
            json!(protocol)
        );
        let text = std::fs::read_to_string(&at.path).unwrap();
        for marker in [
            "tool-input-marker",
            "working-marker",
            "tool-arguments-marker",
            "tool-result-marker",
            "done-marker",
        ] {
            assert_eq!(text.matches(marker).count(), 1, "{marker}: {text}");
        }
        session.rollback_before(call);
        save(&session, protocol.len(), &protocol);
        assert_eq!(Store::read(&at).unwrap().session, session);
        assert_eq!(
            serde_json::to_value(messages::read(&at).unwrap().unwrap().entries).unwrap(),
            json!(protocol)
        );
    }

    #[test]
    fn conversation_and_request_state_roll_back_together() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("state"));
        let workspace = temp.path().join("work");
        let mut session = Session::new();
        session.prompt("original");
        let at = store.save(&workspace, header("s"), &session).unwrap();
        let original = session.clone();
        session.prompt("uncommitted");
        let request = json!({"message":{"id":"input","session":"s","by":null,"at":1,
            "kind":{"type":"request","value":{"direction":"to_model","request":"user"}},
            "body":[{"type":"text","value":"uncommitted"}]},"parent":null,"native_call":null});
        let result = store.save_with(&workspace, header("s"), &session, |tx, at| {
            messages::write_in(tx, at, &at.session, 0, std::slice::from_ref(&request))?;
            anyhow::bail!("interrupted before commit")
        });
        assert!(result.is_err());
        assert_eq!(Store::read(&at).unwrap().session, original);
        assert!(messages::read(&at).unwrap().unwrap().entries.is_empty());

        // A multi-fact save has one newline boundary; its torn tail publishes nothing.
        use std::io::Write;
        let prefix = std::fs::read(&at.path).unwrap();
        let mut torn = std::fs::OpenOptions::new()
            .append(true)
            .open(&at.path)
            .unwrap();
        torn.write_all(b"[{\"type\":\"input\",\"id\":1,\"entry\":1,\"at\":1,\"body\":[{\"type\":\"text\",\"value\":\"uncommitted\"}]},").unwrap();
        torn.write_all(&vec![b'x'; 16_384]).unwrap();
        assert_eq!(Store::read(&at).unwrap().session, original);
        assert!(messages::read(&at).unwrap().unwrap().entries.is_empty());
        store
            .save_with(&workspace, header("s"), &session, |tx, at| {
                messages::write_in(tx, at, &at.session, 0, &[request])
            })
            .unwrap();
        assert_eq!(Store::read(&at).unwrap().session, session);
        assert_eq!(messages::read(&at).unwrap().unwrap().entries.len(), 1);
        assert!(std::fs::read(&at.path).unwrap().starts_with(&prefix));
    }

    #[test]
    fn pruning_multiple_sessions_removes_children_and_keeps_active_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("state"));
        let workspace = temp.path().join("work");
        let mut expected = None;
        for id in ["old-a", "old-b", "active"] {
            let session = Session::from_messages(vec![
                Message::user("shared input"),
                Message::assistant_text(id),
            ]);
            store.save(&workspace, header(id), &session).unwrap();
            if id == "active" {
                expected = Some(session);
            }
        }
        store
            .save_subagent(
                "old-a",
                "child",
                &workspace,
                "m",
                1,
                &Session::with_prompt("child input"),
            )
            .unwrap();
        let _claim = store.claim(&workspace, "active").unwrap();
        store.forget_older_than(std::time::Duration::ZERO, None);
        for removed in ["old-a", "old-b", "child"] {
            assert!(
                !store.path_of(&workspace, removed).exists(),
                "{removed} must be removed"
            );
        }
        let at = store.path_of(&workspace, "active");
        assert_eq!(Store::read(&at).unwrap().session, expected.unwrap());
        assert_eq!(bucket_locations(&store.dir_of(&workspace)).len(), 1);
    }
}
