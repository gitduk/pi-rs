//! Transcripts on disk: where a session's file lives, what a resumed one is
//! read back as, and the bucket layout that keeps two workspaces apart.

use std::collections::HashSet;
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
    #[serde(flatten, default)]
    pub session: Session,
}

// A save as its parts, borrowed, so serializing never needs a `Stored`
// clone — `save` runs from inside tool calls, several subagents at once.
#[derive(Serialize)]
struct StoredRef<'a> {
    id: &'a str,
    workspace: String,
    model: &'a str,
    created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(flatten)]
    session: &'a Session,
}

// An archive read only as far as the listing needs: identity fields
// required like `Stored`'s, everything else optional and left unbuilt.
#[derive(Deserialize)]
struct Peek {
    id: String,
    workspace: String,
    // Unused, and required anyway: it's what separates an archive this
    // build can resume from an incompatible one.
    #[allow(dead_code)]
    model: String,
    #[serde(default)]
    created: u64,
    /// What the user calls this session, by `/name` or `--name`. Shallow like
    /// `created`: a listing shows it and never builds the transcript to say it.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    entries: Vec<PeekEntry>,
    // The transcript's size on disk, set once it is read.
    #[serde(skip)]
    bytes: u64,
}

#[derive(Deserialize)]
struct PeekEntry {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    at: u64,
    /// The ask, when this entry is one — what names the session.
    #[serde(default)]
    ask: Option<PeekAsk>,
}

#[derive(Deserialize)]
struct PeekAsk {
    #[serde(default)]
    text: String,
    #[serde(default)]
    shown: Option<String>,
}

impl Peek {
    // When this session was last worked on, which is what `/resume` sorts by.
    fn touched(&self) -> u64 {
        self.entries.last().map_or(self.created, |e| e.at)
    }

    // How many times the user asked: one round each.
    fn rounds(&self) -> usize {
        self.entries.iter().filter(|e| e.ask.is_some()).count()
    }

    // The first thing the user asked, which is what the list shows in place
    // of an id. A `!` command's output is not it.
    fn opening(&self) -> Option<String> {
        self.entries
            .iter()
            .filter_map(|e| e.ask.as_ref())
            .map(|b| b.shown.clone().unwrap_or_else(|| b.text.clone()))
            .find(|t| !t.is_empty())
    }
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
    pub fn into_session(self) -> Session {
        self.session
    }
}

// A path can be absent for a morning as well as for good, so nothing
// younger than this is swept — an unmounted disk costs a delay, not a transcript.
const UNREACHED_KEEP: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

// Just enough of an archive to say which tree it belongs to.
#[derive(Deserialize)]
struct Belongs {
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
    pub path: PathBuf,
}

/// Where transcripts live. Held as a value rather than read from the
/// environment at each call, so tests need no global state to isolate.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
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
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(Claim { _file: file, dir }),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("session {id} is open in another pi, or another lane of this one")
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// A session directory never saved to leaves nothing behind: its lock goes,
/// and the directory with it once nothing else is there.
fn discard_unsaved(dir: &Path) {
    if !dir.join(TRANSCRIPT).exists() {
        let _ = std::fs::remove_file(dir.join(LOCK));
        let _ = std::fs::remove_dir(dir);
    }
}

/// The same for a sweep: a lock some pi still holds stays where it is.
pub(crate) fn discard_abandoned(dir: &Path) {
    let free = std::fs::File::open(dir.join(LOCK)).map_or(true, |f| f.try_lock().is_ok());
    if free {
        discard_unsaved(dir);
    }
}

// Beside a session's transcript: the subagents its turns called, one file each.
const SUBAGENTS: &str = "subagents";

// One transcript, written whole. The rename means a crash mid-write cannot
// leave a truncated one.
fn write(
    path: &Path,
    id: &str,
    workspace: &Path,
    model: &str,
    name: Option<&str>,
    created: u64,
    session: &Session,
) -> Result<PathBuf> {
    std::fs::create_dir_all(path.parent().expect("a session directory"))?;
    let tmp = path.with_extension("json.tmp");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The transcript holds prompts and file contents. Chmodded before
        // the write, not after: no moment when the data sits world-readable.
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    // Serialized by reference: a transcript is megabytes by the end, and
    // cloning it plus buffering would cost every parallel subagent a copy.
    let stored = StoredRef {
        id,
        workspace: workspace.display().to_string(),
        model,
        created,
        name,
        session,
    };
    use std::io::Write;
    let mut out = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut out, &stored)?;
    out.flush()?;
    drop(out);
    std::fs::rename(&tmp, path)?;
    Ok(path.to_path_buf())
}

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
    let text = std::fs::read_to_string(transcript).ok()?;
    serde_json::from_str(&text).ok()
}

// Which workspace a bucket belongs to, read off its first transcript.
fn workspace_of(transcripts: &[PathBuf]) -> Option<String> {
    transcripts
        .iter()
        .find_map(|p| belongs(p).map(|b| b.workspace))
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    // The directory one workspace's transcripts live in.
    fn dir_of(&self, workspace: &Path) -> PathBuf {
        self.root.join(key_of(workspace))
    }

    /// What `k` recalls in this workspace, kept beside the transcripts
    /// since it answers to the same key and expires with them.
    ///
    /// Per workspace, not per session: what you're reaching back for is
    /// usually something typed before `/new`.
    pub fn history_path(&self, workspace: &Path) -> PathBuf {
        self.dir_of(workspace).join("history")
    }

    /// Where a session's journal is written, beside its transcript, so
    /// dropping the session drops the record of how it went with it.
    pub fn journal_path(&self, workspace: &Path, id: &str) -> PathBuf {
        self.dir_of(workspace)
            .join(tool::state::file_stem(id))
            .join(super::journal::JOURNAL_FILE)
    }

    /// The tree every bucket sits in, for the sweeps that walk all of them.
    pub fn root(&self) -> &Path {
        &self.root
    }

    // Every bucket with its transcripts; the sweep and the removal walk
    // the same tree and differ only in what they do with each bucket.
    fn buckets(&self) -> Vec<(PathBuf, Vec<PathBuf>)> {
        let Ok(dirs) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        dirs.flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .map(|bucket| {
                let transcripts = bucket_transcripts(&bucket);
                (bucket, transcripts)
            })
            .collect()
    }

    /// Where one session's transcript is, written or not yet. `/status` names
    /// it: a transcript nobody can find is one nobody reads back when a run
    /// goes wrong.
    pub fn path_of(&self, workspace: &Path, id: &str) -> PathBuf {
        self.dir_of(workspace)
            .join(tool::state::file_stem(id))
            .join(TRANSCRIPT)
    }

    /// Hold the session `id` saved under `workspace` for this process, so no
    /// other pi saves over it meanwhile; refused while one already holds it.
    pub fn claim(&self, workspace: &Path, id: &str) -> Result<Claim> {
        claim_dir(self.dir_of(workspace).join(tool::state::file_stem(id)), id)
    }

    /// Claim `id` under `workspace`, then read it: read after the claim, it is
    /// the last save, and no other pi makes another while it is held. One
    /// saved under another workspace moves here first, so it is kept once.
    pub fn take(&self, workspace: &Path, id: &str) -> Result<(Stored, Claim)> {
        let mut claim = self.claim(workspace, id)?;
        if !claim.dir.join(TRANSCRIPT).is_file() {
            claim = self.bring(claim, id)?;
        }
        Ok((Self::read(&claim.dir.join(TRANSCRIPT))?, claim))
    }

    // Move the newest copy of `id` filed elsewhere to where `here` holds,
    // whole, once nothing else holds it; the claim now covers it there.
    fn bring(&self, here: Claim, id: &str) -> Result<Claim> {
        let dir = here.dir.clone();
        let stem = tool::state::file_stem(id);
        let modified = |d: &PathBuf| {
            std::fs::metadata(d.join(TRANSCRIPT))
                .and_then(|m| m.modified())
                .ok()
        };
        let from = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join(&stem))
            .filter(|d| *d != dir && d.join(TRANSCRIPT).is_file())
            .max_by_key(modified)
            .with_context(|| format!("no session `{id}` in {}", self.root.display()))?;
        let mut held = claim_dir(from.clone(), id)?;
        // Nothing saved there yet, so letting go of it frees the name; one
        // rename then moves all of it or none.
        drop(here);
        std::fs::rename(&from, &dir)
            .with_context(|| format!("cannot move session `{id}` here from {}", from.display()))?;
        held.dir = dir;
        Ok(held)
    }

    /// `created` is the caller's because it is set once and never changes.
    /// Reading it back off disk here meant parsing the whole transcript to
    /// recover one integer — on every turn, growing with the session it saved.
    pub fn save(
        &self,
        id: &str,
        workspace: &Path,
        model: &str,
        name: Option<&str>,
        created: u64,
        session: &Session,
    ) -> Result<PathBuf> {
        let path = self.path_of(workspace, id);
        write(&path, id, workspace, model, name, created, session)
    }

    /// Where a subagent's transcript is: inside its parent's session, so
    /// `/resume` never offers it and dropping the parent drops it too.
    pub fn subagent_path(&self, workspace: &Path, parent: &str, id: &str) -> PathBuf {
        self.dir_of(workspace)
            .join(tool::state::file_stem(parent))
            .join(SUBAGENTS)
            .join(format!("{}.json", tool::state::file_stem(id)))
    }

    /// File a subagent's transcript under the session that called it.
    pub fn save_subagent(
        &self,
        parent: &str,
        id: &str,
        workspace: &Path,
        model: &str,
        created: u64,
        session: &Session,
    ) -> Result<PathBuf> {
        let path = self.subagent_path(workspace, parent, id);
        write(
            &path,
            id,
            workspace,
            model,
            Some("subagent"),
            created,
            session,
        )
    }

    /// How far every saved session has got, across all workspaces, read
    /// without building a single transcript.
    pub fn progress(&self) -> Vec<Progress> {
        self.buckets()
            .into_iter()
            .flat_map(|(_, t)| t)
            .filter_map(|path| {
                let body = std::fs::read_to_string(&path).ok()?;
                let peek: Peek = serde_json::from_str(&body).ok()?;
                Some(Progress {
                    last: peek.entries.last()?.id,
                    touched: peek.touched(),
                    created: peek.created,
                    id: peek.id,
                    path,
                })
            })
            .collect()
    }

    /// Read the transcript at `path`, as `progress` names it.
    pub fn read(path: &Path) -> Result<Stored> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok(serde_json::from_str(&body)?)
    }

    // A transcript by id, from whichever bucket has it: what a test asks
    // after moving things around. A run reads its own through `take`.
    #[cfg(test)]
    pub(crate) fn load(&self, id: &str) -> Result<Stored> {
        let stem = tool::state::file_stem(id);
        let mut match_: Option<PathBuf> = None;
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(&stem).join(TRANSCRIPT);
                if candidate.is_file() {
                    match_ = Some(candidate);
                    break;
                }
            }
        }
        let Some(path) = match_ else {
            return Err(anyhow::anyhow!(
                "no session `{id}` in {}",
                self.root.display()
            ));
        };
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("no session `{id}` at {}", path.display()))?;
        Ok(serde_json::from_str(&body)?)
    }

    // Every session recorded for this workspace, newest first, read as
    // shallowly as the answer allows.
    fn peek(&self, workspace: &Path) -> Vec<Peek> {
        let want = workspace.display().to_string();
        let mut found: Vec<Peek> = bucket_transcripts(&self.dir_of(workspace))
            .into_iter()
            .filter_map(|path| {
                let body = std::fs::read_to_string(&path).ok()?;
                match serde_json::from_str::<Peek>(&body) {
                    Ok(peek) => (peek.workspace == want).then_some(Peek {
                        bytes: body.len() as u64,
                        ..peek
                    }),
                    Err(e) => {
                        tracing::warn!(
                            target: "pi::session",
                            path = %path.display(),
                            error = %e,
                            "unreadable transcript skipped"
                        );
                        None
                    }
                }
            })
            .collect();
        let mut seen = HashSet::new();
        found.retain(|p| seen.insert(p.id.clone()));
        // Newest by last activity, not creation — `/resume` wants where you
        // left off. Ties break by id, since a stamp is only seconds.
        found.sort_by(|a, b| b.touched().cmp(&a.touched()).then_with(|| b.id.cmp(&a.id)));
        found
    }

    /// Sweeps buckets whose workspace is gone — a removed worktree, a
    /// deleted checkout — since `/resume` has no other way back to them.
    ///
    /// Reachability alone isn't safe: an unmounted disk looks gone too. Age
    /// is what tells the two apart.
    pub fn prune(&self) {
        self.prune_older_than(UNREACHED_KEEP);
    }

    /// Deletes every session not worked on for `keep` but `spare`, the one
    /// about to be resumed.
    pub fn forget_older_than(&self, keep: std::time::Duration, spare: Option<&str>) {
        let spare = spare.map(tool::state::file_stem);
        for (bucket, transcripts) in self.buckets() {
            for transcript in &transcripts {
                let Some(dir) = transcript.parent() else {
                    continue;
                };
                let spared = spare.as_deref().is_some_and(|s| dir.ends_with(s));
                if !spared && crate::older_than(transcript, keep) {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
            // Only an empty one goes; one still holding anything stays.
            let _ = std::fs::remove_dir(&bucket);
        }
    }

    // The same, against a stated age rather than the constant — a test that
    // waits a month is not a test.
    fn prune_older_than(&self, keep: std::time::Duration) {
        for (bucket, transcripts) in self.buckets() {
            // No transcripts here means nothing says whether the tree is
            // gone; `remove_dir` only takes a genuinely empty bucket.
            if transcripts.is_empty() {
                let _ = std::fs::remove_dir(&bucket);
                continue;
            }
            let recent = transcripts.iter().any(|p| !crate::older_than(p, keep));
            if recent {
                continue;
            }
            // Per transcript, not per bucket: two trees can fold to one
            // bucket, so one going away must not take the other's sessions.
            let mut all_gone = !transcripts.is_empty();
            for transcript in &transcripts {
                match workspace_of(std::slice::from_ref(transcript)) {
                    Some(ws) if !Path::new(&ws).is_dir() => {
                        if let Some(dir) = transcript.parent() {
                            let _ = std::fs::remove_dir_all(dir);
                        }
                    }
                    _ => all_gone = false,
                }
            }
            if all_gone {
                let _ = std::fs::remove_dir_all(&bucket);
            }
        }
    }
    /// Removes the records under `root` immediately, rather than waiting
    /// for the sweep's month of grace. Returns how many transcripts went.
    ///
    /// One session at a time: a bucket goes whole only when every
    /// transcript in it was recorded under `root`.
    pub fn drop_under(&self, root: &Path) -> usize {
        let mut dropped = 0;
        for (bucket, transcripts) in self.buckets() {
            // Each transcript names the workspace it was saved under; split
            // the bucket's sessions into ours and someone else's.
            let mut owed: Vec<&Path> = Vec::new();
            let mut shared = false;
            for transcript in &transcripts {
                let Some(belongs) = belongs(transcript) else {
                    continue;
                };
                if Path::new(&belongs.workspace).starts_with(root) {
                    owed.push(transcript.parent().expect("a transcript has a session dir"));
                } else {
                    shared = true;
                }
            }
            if owed.is_empty() {
                continue;
            }
            if shared {
                // The bucket holds another tree's sessions too, and its
                // recall cannot be told apart; only ours go.
                for session_dir in owed {
                    if std::fs::remove_dir_all(session_dir).is_ok() {
                        dropped += 1;
                    }
                }
            } else {
                // Every session here belongs to `root`: the whole bucket —
                // recall beside the transcripts — goes.
                dropped += transcripts.len();
                let _ = std::fs::remove_dir_all(&bucket);
            }
        }
        dropped
    }

    /// Every session `/resume` can name for this workspace, newest first,
    /// reduced to what the list and its completion show: the id it is named
    /// by, the name the user gave it, and its first prompt.
    pub fn choices(&self, workspace: &Path) -> Vec<ResumeChoice> {
        self.peek(workspace)
            .into_iter()
            .map(|p| ResumeChoice {
                prompt: p.opening().unwrap_or_default(),
                rounds: p.rounds(),
                touched: p.touched(),
                bytes: p.bytes,
                id: p.id,
                name: p.name,
            })
            .collect()
    }

    /// Most recent session recorded for this workspace, taken as `take` does
    /// and read in full — it is about to be resumed, which is the one time
    /// the whole transcript is wanted.
    pub fn take_latest(&self, workspace: &Path) -> Result<(Stored, Claim)> {
        let newest = self
            .peek(workspace)
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
        assert!(err.contains("another pi"), "{err}");
        drop(held);
        let dir = store.path_of(ws, "a").parent().unwrap().to_path_buf();
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

    fn log_with(messages: Vec<Message>) -> Session {
        Session::from_messages(messages)
    }

    // Sets what `touched()` reads. Forced through raw JSON because an
    // entry's stamp is the session's to set, not a caller's.
    fn touched_at(store: &Store, workspace: &Path, id: &str, at: u64) {
        let path = store.path_of(workspace, id);
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["entries"].as_array_mut().unwrap().last_mut().unwrap()["at"] = json!(at);
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
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
            store.save(id, at, "test-model", None, 7, &log).unwrap();
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
                .save(id, ws.path(), "test-model", None, 7, &log)
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
            .save("live", &live, "test-model", None, 7, &log)
            .unwrap();
        store
            .save("gone", &gone, "test-model", None, 7, &log)
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

    // `/worktree rm` drops a tree's buckets immediately rather than
    // waiting for the sweep; a sibling tree's records must survive.
    #[test]
    fn drop_under_removes_the_buckets_of_one_checkout_and_no_others() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("store"));
        let home = tempfile::tempdir().unwrap();
        let log = log_with(vec![Message::user("hi")]);
        let save = |id: &str, at: &std::path::Path| {
            store.save(id, at, "test-model", None, 7, &log).unwrap();
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
            .save("mine", &tree, "test-model", None, 7, &log)
            .unwrap();
        store
            .save("theirs", &sibling, "test-model", None, 7, &log)
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
                "t1",
                std::path::Path::new("/w"),
                "test-model",
                Some("the flaky test"),
                7,
                &log,
            )
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "a transcript holds prompts and file contents"
            );
        }
        // The session flattens to the top level: no nested `log` key.
        let body = std::fs::read_to_string(&path).unwrap();
        let flat = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let flat = flat.as_object().unwrap();
        assert!(
            flat.contains_key("entries"),
            "session must flatten to the top level"
        );
        assert!(
            !flat.contains_key("log"),
            "the nested `log` key must be gone"
        );
        let back = store.load("t1").unwrap();
        assert_eq!(back.model, "test-model");
        assert_eq!(back.name.as_deref(), Some("the flaky test"));
        assert_eq!(back.into_session(), log);

        // The listing reads the name without opening the transcript: a field
        // `Peek` does not name is a field the row would not have.
        let listed = store.choices(std::path::Path::new("/w"));
        assert_eq!(listed[0].name.as_deref(), Some("the flaky test"));
    }

    #[test]
    fn a_transcript_with_tool_traffic_survives_the_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("go"), call("read"), results()]);
        store
            .save("t", std::path::Path::new("/w"), "m", None, 1, &log)
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
            .save("t", std::path::Path::new("/w"), "m", None, 1, &log)
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
            .save("old", std::path::Path::new("/a"), "m", None, 1, &log)
            .unwrap();
        store
            .save("other", std::path::Path::new("/b"), "m", None, 1, &log)
            .unwrap();
        // A second session in the same workspace, named `new` to sort after
        // `old` by id — so only recency can put it first.
        store
            .save("new", std::path::Path::new("/a"), "m", None, 1, &log)
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

    // Taken from another workspace, a session moves rather than forks: one
    // copy, under the workspace now using it — never while another pi has it.
    #[test]
    fn a_session_taken_elsewhere_moves_with_everything_beside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        let log = log_with(vec![Message::user("x")]);
        store.save("s", a, "m", None, 1, &log).unwrap();
        store.save_subagent("s", "s-sub", a, "m", 2, &log).unwrap();

        let held = store.claim(a, "s").unwrap();
        let err = store.take(b, "s").err().unwrap().to_string();
        assert!(err.contains("another pi"), "{err}");
        assert!(store.path_of(a, "s").is_file(), "refused, nothing moved");
        drop(held);

        let (stored, _claim) = store.take(b, "s").unwrap();
        assert_eq!(stored.id, "s");
        assert!(store.path_of(b, "s").is_file());
        assert!(store.subagent_path(b, "s", "s-sub").is_file());
        assert!(!store.path_of(a, "s").parent().unwrap().exists());
    }

    #[test]
    fn a_subagent_is_filed_inside_its_parent_and_never_offered_back() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let w = std::path::Path::new("/w");
        let log = log_with(vec![Message::user("x")]);
        store.save("p", w, "m", None, 1, &log).unwrap();
        let at = store
            .save_subagent("p", "p-subagent-0", w, "m", 2, &log)
            .unwrap();

        assert!(at.starts_with(store.path_of(w, "p").parent().unwrap()));
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
            .save("a", std::path::Path::new("/w"), "m", None, 1, &log)
            .unwrap();
        store
            .save("b", std::path::Path::new("/w"), "m", None, 1, &log)
            .unwrap();
        store
            .save("c", std::path::Path::new("/other"), "m", None, 1, &log)
            .unwrap();

        store
            .save("z", std::path::Path::new("/w"), "m", None, 1, &log)
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
            .save("a", std::path::Path::new("/w"), "m", None, 1, &asked)
            .unwrap();
        store
            .save(
                "b",
                std::path::Path::new("/w"),
                "m",
                None,
                1,
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
            .save("t", std::path::Path::new("/w"), "m", None, 1, &log)
            .unwrap();

        // A session is a directory in the bucket, holding the transcript and
        // the journal that recorded it.
        let bucket = tmp.path().join("-w").join("t").join("session.json");
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
