//! Workspace conversation databases, legacy imports and per-session artifacts.

mod database;
pub use database::Location;
pub use database::append_draft;
pub use rusqlite::Transaction;
mod log;
pub mod messages;

use std::io::{BufRead, Read};
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
    fn header(&self) -> Header<'_> {
        Header {
            id: &self.id,
            model: &self.model,
            name: self.name.as_deref(),
            created: self.created,
            spent: self.spent,
        }
    }

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

// Just enough of an archive to say which tree it belongs to.
#[derive(Deserialize)]
struct Belongs {
    id: String,
    workspace: String,
}

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
const TRANSCRIPT: &str = "session.json";
const LOCK: &str = ".lock";
const BACKGROUND: &str = "background.json";

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
    let database = dir.parent().unwrap_or(dir).join(database::FILE);
    let saved = database::summaries(&database)
        .map(|sessions| {
            sessions.iter().any(|s| {
                std::ffi::OsStr::new(&tool::state::file_stem(&s.id))
                    == dir.file_name().unwrap_or_default()
            })
        })
        .unwrap_or(true);
    if !dir.join(TRANSCRIPT).exists() && !saved {
        let _ = std::fs::remove_file(dir.join(LOCK));
        if std::fs::remove_dir(dir).is_ok()
            && let Some(workspace) = dir.parent()
        {
            let _ = std::fs::remove_dir(workspace);
        }
    }
}

/// The same for a sweep: a lock some pi still holds stays where it is.
pub(crate) fn discard_abandoned(dir: &Path) {
    let lock = std::fs::File::open(dir.join(LOCK)).ok();
    if lock.as_ref().is_some_and(|f| f.try_lock().is_err()) {
        return;
    }
    // Held through the unlink, so no claim takes the lock in between and
    // then holds a file that is no longer there.
    discard_unsaved(dir);
    drop(lock);
}

/// Whether some pi holds the session in `dir`, this one included: what a
/// sweep leaves alone.
pub(crate) fn held(dir: &Path) -> bool {
    std::fs::File::open(dir.join(LOCK)).is_ok_and(|f| f.try_lock().is_err())
}

// Make a rename or unlink in `dir` survive a crash. Where a directory cannot
// be opened to sync, the rename alone is what there is.
#[cfg(test)]
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

// Legacy subagent records, imported with their parent.
const SUBAGENTS: &str = "subagents";

// One transcript per session directory in the bucket; asking for the
// transcript inside each entry skips the bucket's own `history` file.
fn bucket_transcripts(bucket: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(bucket) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join(TRANSCRIPT))
        .filter(|p| p.is_file())
        .collect()
}

// The workspace a transcript says it was saved under: the record inside
// is authoritative, since the bucket name only encodes the path lossily.
fn belongs(transcript: &Path) -> Option<Belongs> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(transcript).ok()?);
    let mut text = String::new();
    reader.read_line(&mut text).ok()?;
    if !log::is_log(&text) {
        reader.read_to_string(&mut text).ok()?;
    }
    serde_json::from_str(&text).ok()
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            version: Default::default(),
        }
    }

    // The directory one workspace's transcripts live in.
    fn dir_of(&self, workspace: &Path) -> PathBuf {
        self.root.join(key_of(workspace))
    }

    /// Session artifacts kept outside the database: its claim and diagnostic files.
    pub fn session_dir(&self, workspace: &Path, id: &str) -> PathBuf {
        self.dir_of(workspace).join(tool::state::file_stem(id))
    }

    /// What this session's row last showed, if anything ever did.
    pub fn shown(&self, workspace: &Path, id: &str) -> Option<crate::statusline::Shown> {
        crate::statusline::Shown::read(&self.session_dir(workspace, id))
    }

    /// What `k` recalls in this workspace, kept beside the transcripts
    /// since it answers to the same key and expires with them.
    ///
    /// Per workspace, not per session: what you're reaching back for is
    /// usually something typed in an earlier session.
    pub fn history_path(&self, workspace: &Path) -> PathBuf {
        self.dir_of(workspace).join("history")
    }

    /// Where a session's journal is written, beside its transcript, so
    /// dropping the session drops the record of how it went with it.
    pub fn journal_path(&self, workspace: &Path, id: &str) -> PathBuf {
        self.session_dir(workspace, id)
            .join(super::journal::JOURNAL_FILE)
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

    /// Address a session in the workspace's shared database.
    pub fn path_of(&self, workspace: &Path, id: &str) -> Location {
        Location::new(
            self.dir_of(workspace).join(database::FILE),
            message::SessionId::new(id.to_owned()).expect("session identifier"),
        )
    }

    pub fn filed(&self, workspace: &Path, id: &str) -> bool {
        self.path_of(workspace, id).exists()
            || self.session_dir(workspace, id).join(TRANSCRIPT).is_file()
    }

    pub fn claim(&self, workspace: &Path, id: &str) -> Result<Claim> {
        claim_dir(self.session_dir(workspace, id), id)
    }

    /// Import once under the session claim. Legacy files remain readable backups.
    fn import(&self, workspace: &Path, id: &str) -> Result<()> {
        let at = self.path_of(workspace, id);
        if at.recorded()? {
            return self.archive_legacy(workspace, id);
        }
        let legacy = self.session_dir(workspace, id).join(TRANSCRIPT);
        if !legacy.is_file() {
            return Ok(());
        }
        let stored = Self::read_legacy(&legacy)?;
        anyhow::ensure!(
            stored.id == id,
            "legacy session identifier does not match its address"
        );
        let mut c = database::connect(&at.database)?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        database::write(
            &tx,
            Path::new(&stored.workspace),
            stored.header(),
            &stored.session,
            None,
        )?;
        if let Some(archive) = messages::legacy::read(&legacy.with_file_name("messages"))? {
            messages::write_in(&tx, &at, &at.session, 0, &archive.entries)?;
        }
        let subs = legacy.parent().expect("session directory").join(SUBAGENTS);
        for entry in std::fs::read_dir(subs).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                let child = Self::read_legacy(&entry.path())?;
                database::write(
                    &tx,
                    Path::new(&stored.workspace),
                    child.header(),
                    &child.session,
                    Some(&stored.id),
                )?;
            }
        }
        tx.commit()?;
        self.archive_legacy(workspace, id)?;
        tracing::info!(target:"pi::session",session=id,"legacy conversation imported");
        Ok(())
    }

    fn archive_legacy(&self, workspace: &Path, id: &str) -> Result<()> {
        let dir = self.session_dir(workspace, id);
        for name in [TRANSCRIPT, "messages", SUBAGENTS] {
            let from = dir.join(name);
            if from.exists() {
                std::fs::rename(&from, dir.join(format!("{name}.legacy")))?;
            }
        }
        Ok(())
    }

    fn import_bucket(&self, workspace: &Path) {
        for path in bucket_transcripts(&self.dir_of(workspace)) {
            let Some(belongs) = belongs(&path) else {
                continue;
            };
            if belongs.workspace != workspace.to_string_lossy() {
                continue;
            }
            self.import_available(&belongs);
        }
    }

    fn import_available(&self, belongs: &Belongs) {
        let workspace = Path::new(&belongs.workspace);
        let id = &belongs.id;
        if let Ok(_claim) = self.claim(workspace, id)
            && let Err(error) = self.import(workspace, id)
        {
            tracing::warn!(target:"pi::session",session=id,%error,"legacy conversation not imported");
        }
    }

    pub fn take(&self, workspace: &Path, id: &str) -> Result<(Stored, Claim)> {
        let claim = self.relocate(workspace, id)?;
        Ok((Self::read(&self.path_of(workspace, id))?, claim))
    }

    pub fn relocate(&self, workspace: &Path, id: &str) -> Result<Claim> {
        let here = self.claim(workspace, id)?;
        let destination = self.path_of(workspace, id);
        self.import(workspace, id)?;
        if destination.exists() {
            return Ok(here);
        }
        for bucket in self.buckets() {
            let transcripts = bucket_transcripts(&bucket);
            for transcript in transcripts {
                if let Some(b) = belongs(&transcript)
                    && b.id == id
                {
                    let _source = self.claim(Path::new(&b.workspace), id)?;
                    self.import(Path::new(&b.workspace), id)?;
                }
            }
            let database = bucket.join(database::FILE);
            if let Some(summary) = database::summary(&database, id)?
                && summary.parent.is_none()
            {
                let source = Location::new(database, destination.session.clone());
                let _source = self.claim(Path::new(&summary.workspace), id)?;
                database::relocate(&source, &destination, workspace)?;
                let old = source.artifacts();
                for entry in std::fs::read_dir(&old).into_iter().flatten().flatten() {
                    if entry.file_name() != LOCK
                        && entry.file_name() != TRANSCRIPT
                        && entry.file_name() != "messages"
                        && entry.file_name() != SUBAGENTS
                    {
                        std::fs::rename(entry.path(), here.dir.join(entry.file_name()))?;
                    }
                }
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

    /// The entire persisted state of one execution lands in one transaction.
    pub fn save_with(
        &self,
        workspace: &Path,
        header: Header<'_>,
        session: &Session,
        extra: impl FnOnce(&rusqlite::Transaction<'_>, &Location) -> Result<()>,
    ) -> Result<Location> {
        let at = self.path_of(workspace, header.id);
        let mut c = database::connect(&at.database)?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        database::write(&tx, workspace, header, session, None)?;
        extra(&tx, &at)?;
        tx.commit()?;
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
        let mut c = database::connect(&at.database)?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        database::write(
            &tx,
            workspace,
            Header {
                id,
                model,
                name: Some("subagent"),
                created,
                spent: Default::default(),
            },
            session,
            Some(parent),
        )?;
        tx.commit()?;
        Ok(at)
    }

    pub fn progress(&self) -> Vec<Progress> {
        self.buckets()
            .into_iter()
            .flat_map(|bucket| {
                let database = bucket.join(database::FILE);
                database::summaries(&database)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| s.parent.is_none())
                    .map(move |s| Progress {
                        id: s.id.clone(),
                        last: s.last,
                        created: s.created,
                        touched: s.touched,
                        path: Location::new(
                            database.clone(),
                            message::SessionId::new(s.id).expect("stored session identifier"),
                        ),
                    })
            })
            .collect()
    }

    pub fn read(at: &Location) -> Result<Stored> {
        database::read(at)
    }

    /// Decode a legacy transcript for migration.
    fn read_legacy(path: &Path) -> Result<Stored> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if !log::is_log(&body) {
            // Written before the log: one object holding the whole transcript.
            return Ok(serde_json::from_str(&body)?);
        }
        let (replayed, ledger) = log::replay::<transcript::Entry>(&body);
        Ok(Stored {
            id: ledger.id,
            workspace: ledger.workspace,
            // The last save's, which a resume can change; the ledger's is what
            // the session began on.
            model: replayed.model.unwrap_or(ledger.model),
            created: ledger.created,
            name: replayed.name,
            spent: replayed.spent,
            session: Session::from_parts(replayed.entries, replayed.next, replayed.interrupted),
        })
    }

    #[cfg(test)]
    pub(crate) fn load(&self, id: &str) -> Result<Stored> {
        for bucket in self.buckets() {
            let at = Location::new(
                bucket.join(database::FILE),
                message::SessionId::new(id.into())?,
            );
            if at.exists() {
                return Self::read(&at);
            }
        }
        anyhow::bail!("no session `{id}` in {}", self.root.display())
    }

    pub fn remove(&self, workspace: &Path, id: &str) -> Result<()> {
        let at = self.path_of(workspace, id);
        if at.database.is_file() {
            database::delete(&at)?;
        }
        self.remove_artifacts(workspace, id)?;
        self.changed();
        Ok(())
    }

    fn remove_artifacts(&self, workspace: &Path, id: &str) -> Result<()> {
        let dir = self.session_dir(workspace, id);
        if dir.is_dir() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    fn remove_selected(&self, path: &Path, sessions: &[database::Summary]) -> Result<usize> {
        let ids: Vec<_> = sessions.iter().map(|s| s.id.as_str()).collect();
        database::delete_many(path, &ids)?;
        Ok(sessions
            .iter()
            .filter(|s| {
                self.remove_artifacts(Path::new(&s.workspace), &s.id)
                    .is_ok()
            })
            .count())
    }

    pub fn prune(&self) {
        self.prune_older_than(UNREACHED_KEEP);
    }

    pub fn forget_older_than(&self, keep: std::time::Duration, spare: Option<&str>) {
        self.sweep(keep, |s| spare != Some(s.id.as_str()));
    }

    fn prune_older_than(&self, keep: std::time::Duration) {
        self.sweep(keep, |s| !Path::new(&s.workspace).is_dir());
    }

    fn sweep(&self, keep: std::time::Duration, eligible: impl Fn(&database::Summary) -> bool) {
        self.changed();
        for bucket in self.buckets() {
            let transcripts = bucket_transcripts(&bucket);
            for transcript in transcripts {
                if let Some(b) = belongs(&transcript) {
                    self.import_available(&b);
                }
            }
            let database = bucket.join(database::FILE);
            let selected: Vec<_> = database::summaries(&database)
                .unwrap_or_default()
                .into_iter()
                .filter(|s| {
                    s.parent.is_none()
                        && eligible(s)
                        && !held(&self.session_dir(Path::new(&s.workspace), &s.id))
                        && now().saturating_sub(s.touched) >= keep.as_secs()
                })
                .collect();
            let _ = self.remove_selected(&database, &selected);
        }
    }

    pub fn drop_under(&self, root: &Path) -> usize {
        self.changed();
        let mut dropped = 0;
        for bucket in self.buckets() {
            let transcripts = bucket_transcripts(&bucket);
            for transcript in transcripts {
                if let Some(b) = belongs(&transcript) {
                    self.import_available(&b);
                }
            }
            let database = bucket.join(database::FILE);
            let selected: Vec<_> = database::summaries(&database)
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.parent.is_none() && Path::new(&s.workspace).starts_with(root))
                .collect();
            dropped += self
                .remove_selected(&database, &selected)
                .unwrap_or_default();
        }
        dropped
    }

    pub fn choices(&self, workspace: &Path) -> Vec<ResumeChoice> {
        self.import_bucket(workspace);
        let mut choices: Vec<_> = database::summaries(&self.dir_of(workspace).join(database::FILE))
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.parent.is_none() && s.workspace == workspace.to_string_lossy())
            .map(|s| ResumeChoice {
                id: s.id,
                prompt: s.prompt,
                name: s.name,
                touched: s.touched,
                rounds: s.rounds,
                bytes: s.bytes,
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

        // A sweep leaves a held lock alone and takes an abandoned one.
        let held = store.claim(ws, "a").unwrap();
        super::discard_abandoned(&dir);
        assert!(dir.join(super::LOCK).exists());
        drop(held);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(super::LOCK), "").unwrap();
        super::discard_abandoned(&dir);
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
        database::connect(&path.database)
            .unwrap()
            .execute(
                "UPDATE sessions SET touched=?1 WHERE id=?2",
                rusqlite::params![at, id],
            )
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
            let mode = std::fs::metadata(&path.database)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "a transcript holds prompts and file contents"
            );
        }
        assert!(
            !store
                .session_dir(Path::new("/w"), "t1")
                .join(TRANSCRIPT)
                .exists()
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

    // What the session cost comes back with it; a file from before the field
    // existed reads as nothing spent rather than refusing to load.
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

        let connection = database::connect(&path.database).unwrap();
        connection
            .execute(
                "UPDATE sessions SET spent=?1 WHERE id='s'",
                [serde_json::to_string(&llm::Totals::default()).unwrap()],
            )
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
    }

    // A cached resume list is dropped by this number alone: a
    // write that changes `choices` without moving it leaves the list stale.
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
    fn a_subagent_is_filed_inside_its_parent_and_never_offered_back() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let w = std::path::Path::new("/w");
        let log = log_with(vec![Message::user("x")]);
        store.save(w, header("p", "m", None, 1), &log).unwrap();
        let at = store
            .save_subagent("p", "p-subagent-0", w, "m", 2, &log)
            .unwrap();

        assert_eq!(at.database, store.path_of(w, "p").database);
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

        // Conversations share the workspace database.
        let bucket = tmp.path().join("-w").join("project.db");
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
mod database_tests {
    use super::*;
    use llm::message::{AssistantContent, Message, ToolCall, ToolResult};
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
    fn unfinished_output_survives_retries_and_moves_without_entering_context() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("state"));
        let workspace = temp.path().join("work");
        let mut session = Session::with_prompt("inspect");
        let at = store.save(&workspace, header("s"), &session).unwrap();
        let mut draft = transcript::Draft {
            id: "first-attempt".into(),
            entry: transcript::EntryId(session.next_id()),
            from: 0,
            blocks: vec![message::Block::Text("first ".into())],
        };
        append_draft(&at, &draft).unwrap();
        append_draft(&at, &draft).unwrap();
        draft.from = 1;
        draft.blocks = vec![message::Block::Text("tail".into())];
        append_draft(&at, &draft).unwrap();
        let recovered = Store::read(&at).unwrap().session;
        assert_eq!(
            recovered.context(),
            session.context(),
            "drafts cannot become model input"
        );
        assert_eq!(
            recovered.draft().unwrap().blocks.len(),
            2,
            "a replayed batch is not duplicated"
        );
        draft.blocks = vec![message::Block::Text("different".into())];
        assert!(append_draft(&at, &draft).is_err());
        draft.id = "retry".into();
        draft.from = 0;
        draft.blocks = vec![message::Block::Text("new attempt".into())];
        append_draft(&at, &draft).unwrap();
        session.push_notice("inspected the interrupted run");
        store.save(&workspace, header("s"), &session).unwrap();
        assert_eq!(
            Store::read(&at).unwrap().session.draft(),
            Some(&draft),
            "an unrelated notice cannot discard unfinished output"
        );
        let target_workspace = temp.path().join("other");
        let target = store.path_of(&target_workspace, "s");
        database::relocate(&at, &target, &target_workspace).unwrap();
        assert_eq!(Store::read(&target).unwrap().session.draft(), Some(&draft));
        session.push_assistant(vec![AssistantContent::Text(llm::message::Text {
            text: "complete".into(),
        })]);
        store
            .save(&target_workspace, header("s"), &session)
            .unwrap();
        assert!(Store::read(&target).unwrap().session.draft().is_none());
        assert!(
            append_draft(&target, &draft).is_err(),
            "a stale batch cannot resurrect a completed reply"
        );
        let prompt = session.prompt("another task");
        store
            .save(&target_workspace, header("s"), &session)
            .unwrap();
        draft.id = "another-attempt".into();
        draft.entry = transcript::EntryId(session.next_id());
        append_draft(&target, &draft).unwrap();
        session.rollback_before(prompt);
        store
            .save(&target_workspace, header("s"), &session)
            .unwrap();
        assert!(Store::read(&target).unwrap().session.draft().is_none());
    }

    #[test]
    fn shared_database_isolates_sessions_and_rewind_keeps_original_content() {
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
        assert_eq!(a.database, b.database);
        assert_eq!(Store::read(&a).unwrap().session, first);
        assert_eq!(Store::read(&b).unwrap().session, second);
        let before = first.entries().len();
        first.rollback_before(first.entries()[2].id());
        first.prompt("replacement");
        store.save(&workspace, header("a"), &first).unwrap();
        assert_eq!(Store::read(&a).unwrap().session, first);
        let c = database::connect(&a.database).unwrap();
        let retained: usize = c
            .query_row("SELECT COUNT(*) FROM messages WHERE session='a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            retained,
            before + 1,
            "rewinding only changes context membership"
        );
        drop(c);
        store.remove(&workspace, "a").unwrap();
        assert_eq!(Store::read(&b).unwrap().session, second);
        assert!(!a.exists());
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
            messages::write_in(tx, at, &at.session, 0, &[request])?;
            anyhow::bail!("interrupted before commit")
        });
        assert!(result.is_err());
        assert_eq!(Store::read(&at).unwrap().session, original);
        assert!(messages::read(&at).unwrap().is_none());
    }

    #[test]
    fn legacy_import_preserves_reply_order_reasoning_compaction_and_request_identity() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("state"));
        let workspace = temp.path().join("work");
        let session = Session::from_messages(vec![
            Message::user("inspect"),
            Message::Assistant {
                content: vec![
                    AssistantContent::Reasoning(message::Reasoning {
                        id: Some("r".into()),
                        by: Some("m".into()),
                        content: vec![message::ReasoningContent::Text {
                            text: "reason".into(),
                            signature: Some("signature".into()),
                        }],
                    }),
                    AssistantContent::Text(message::Text {
                        text: "reading".into(),
                    }),
                    AssistantContent::ToolCall(ToolCall {
                        id: "c".into(),
                        name: "read".into(),
                        args: json!({"path":"a.rs"}),
                    }),
                ],
            },
            Message::tool_results(vec![ToolResult::text("c", "read", "body")]),
        ]);
        let mut session = session;
        session.record(transcript::Compaction {
            omissions: vec![transcript::Omission {
                entry: session.entries().last().unwrap().id(),
                block: None,
                notice: "omitted".into(),
            }],
            ..Default::default()
        });
        let dir = store.session_dir(&workspace, "s");
        let legacy = dir.join(TRANSCRIPT);
        log::write(&legacy, &workspace, header("s"), &session).unwrap();
        let requests = dir.join("messages");
        std::fs::create_dir_all(&requests).unwrap();
        let input = json!({"message":{"id":"original-input","session":"s","by":null,"at":1,
            "kind":{"type":"request","value":{"direction":"to_model","request":"user"}},
            "body":[{"type":"text","value":"inspect"}]},"parent":null,"native_call":null});
        std::fs::write(
            requests.join("0-1.json"),
            serde_json::to_vec(&vec![input]).unwrap(),
        )
        .unwrap();
        std::fs::write(
            requests.join("index.json"),
            r#"{"format":1,"session":"s","segments":[{"from":0,"to":1}]}"#,
        )
        .unwrap();
        let (stored, claim) = store.take(&workspace, "s").unwrap();
        assert_eq!(stored.session, session);
        let at = store.path_of(&workspace, "s");
        assert_eq!(
            messages::read(&at).unwrap().unwrap().entries[0]
                .message
                .id
                .as_str(),
            "original-input"
        );
        assert!(!legacy.exists());
        assert!(dir.join("session.json.legacy").exists());
        drop(claim);
        let (again, _claim) = store.take(&workspace, "s").unwrap();
        assert_eq!(again.session, session, "migration is idempotent");
        let c = database::connect(&at.database).unwrap();
        let inspect: usize = c
            .query_row(
                "SELECT COUNT(*) FROM contents WHERE body=?1",
                [r#"{"type":"text","value":"inspect"}"#],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            inspect, 1,
            "protocol and conversation reference the same body block"
        );
    }
    #[test]
    fn a_version_one_database_gains_an_index_for_cascading_entry_deletions() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("state"));
        let workspace = temp.path().join("work");
        let session = Session::with_prompt("retained conversation");
        let at = store.save(&workspace, header("old"), &session).unwrap();
        {
            let connection = database::connect(&at.database).unwrap();
            let indexes: Vec<String> = connection.prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name IN ('context_entries','sessions') AND sql IS NOT NULL").unwrap()
                .query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            for index in indexes {
                connection
                    .execute_batch(&format!("DROP INDEX \"{index}\";"))
                    .unwrap();
            }
            connection
                .execute_batch("DROP TABLE draft_parts; DROP TABLE drafts; PRAGMA user_version=1;")
                .unwrap();
        }
        assert_eq!(Store::read(&at).unwrap().session, session);
        let connection = database::connect(&at.database).unwrap();
        let plan: String = connection.query_row("EXPLAIN QUERY PLAN SELECT rowid FROM context_entries WHERE session=?1 AND entry_id=?2", rusqlite::params!["old", 0], |row| row.get(3)).unwrap();
        assert!(
            plan.contains("entry_id=?"),
            "entry deletion must use both key columns: {plan}"
        );
        let family_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM sessions WHERE parent=?1",
                ["old"],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            family_plan.contains("parent=?"),
            "child lookup must use the parent index: {family_plan}"
        );
        drop(connection);
        store.remove(&workspace, "old").unwrap();
        assert!(!at.exists());
    }

    #[test]
    fn pruning_multiple_sessions_retains_shared_content_and_active_sessions() {
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
        let connection = database::connect(&at.database).unwrap();
        let remaining: usize = connection
            .query_row("SELECT COUNT(*) FROM contents", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            remaining, 2,
            "only the active conversation's shared input and answer remain"
        );
    }
}
