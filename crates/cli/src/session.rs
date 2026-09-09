use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent::session::Session;
/// The clock this crate dates transcripts by — the session's own, so a file
/// and the entries inside it are never stamped by two.
pub use agent::session::now;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Stored {
    pub id: String,
    pub workspace: String,
    /// Which model this session ran, as the endpoint names it. One name for
    /// it everywhere now: the archive used to spell it differently from the
    /// config and from the wire, and was the one place a reader could not tell
    /// the three apart.
    pub model: String,
    /// When the session began. Rewritten on every save it would be a
    /// last-touched time under a name that says otherwise.
    pub created: u64,
    /// What the user calls this session. Ids are a timestamp and a pid, which
    /// nobody recognises a week later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten, default)]
    pub session: Session,
}

/// A save as its parts, borrowed. `Stored` owns a deep copy of the session;
/// serializing through this keeps the same file without ever building one —
/// `save` is called from inside tool calls, several subagents at once.
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

// An archive read only as far as the listing needs.
//
// The identity fields are required, exactly as `Stored` requires them, so a
// session that lists is a session that loads. Everything under them is
// optional and shallow: `serde` skips a field no struct here names without
// building it, and what it skips is the whole of the transcript.
#[derive(Deserialize)]
struct Peek {
    id: String,
    workspace: String,
    /// Unused, and required anyway: it is what separates an archive this build
    /// can resume from one written before the provider rename.
    #[allow(dead_code)]
    model: String,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    entries: Vec<PeekEntry>,
}

#[derive(Deserialize)]
struct PeekEntry {
    #[serde(default)]
    at: u64,
    #[serde(default)]
    body: Option<PeekBody>,
}

#[derive(Deserialize)]
struct PeekBody {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    shown: Option<String>,
}

impl Peek {
    /// When this session was last worked on, which is what `/resume` sorts by.
    fn touched(&self) -> u64 {
        self.entries.last().map_or(self.created, |e| e.at)
    }

    /// The first thing the user asked, which is what the list shows in place
    /// of an id. A `!` command's output is not it.
    fn opening(&self) -> Option<String> {
        self.entries
            .iter()
            .filter_map(|e| e.body.as_ref())
            .find(|b| b.kind == "prompt")
            .map(|b| b.shown.clone().unwrap_or_else(|| b.text.clone()))
            .filter(|t| !t.is_empty())
    }
}

/// A saved session as the resume list and its completion see it: the id it is
/// named by, and the first thing the user asked it, which is what it is known
/// by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeChoice {
    pub id: String,
    pub prompt: String,
    pub created: u64,
}

impl Stored {
    pub fn into_session(self) -> Session {
        self.session
    }
}

/// A session whose workspace is gone is unreachable, but a path can be absent
/// for a morning as well as for good. Nothing younger than this is swept, so
/// an unmounted disk costs a delay and never a transcript.
const UNREACHED_KEEP: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Just enough of an archive to say which tree it belongs to.
#[derive(Deserialize)]
struct Belongs {
    workspace: String,
}

/// Where transcripts live. Held as a value rather than read from the
/// environment at each call, so tests need no global state to isolate.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Default for Store {
    /// Outside the workspace: transcripts are the agent's state, not the
    /// project's, and a stray file in a repo is one the user has to clean up.
    fn default() -> Self {
        Self::new(
            tools::state::dir()
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

/// The transcripts one bucket holds, one per session in that workspace.
fn bucket_transcripts(bucket: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(bucket) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join("session.json"))
        .filter(|p| p.is_file())
        .collect()
}

/// Which workspace a bucket belongs to, read off its first transcript: the
/// bucket name is a lossy encoding of the path and would guess wrong.
fn workspace_of(transcripts: &[PathBuf]) -> Option<String> {
    transcripts.iter().find_map(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        let belongs: Belongs = serde_json::from_str(&text).ok()?;
        Some(belongs.workspace)
    })
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory one workspace's transcripts live in.
    fn dir_of(&self, workspace: &Path) -> PathBuf {
        self.root.join(tools::state::key_of(workspace))
    }

    /// What `k` recalls in this workspace. Beside the transcripts rather than
    /// in a tree of its own: it answers to the same key, and it stops meaning
    /// anything at the same moment they do.
    ///
    /// Per workspace and not per session on purpose — the line you are
    /// reaching back for is usually one you typed before `/new`.
    pub fn history_path(&self, workspace: &Path) -> PathBuf {
        self.dir_of(workspace).join("history")
    }

    /// Where one repository's shelf lives, in the bucket its main checkout
    /// owns, so every checkout of the repository shares the file. Outside
    /// git the checkout path itself is the bucket.
    pub fn memory_path(&self, workspace: &Path) -> PathBuf {
        let repo = crate::worktree::home(workspace).unwrap_or_else(|| workspace.to_path_buf());
        self.dir_of(&repo).join("memory.json")
    }

    /// Where a session's journal is written. Beside its transcript, so that
    /// dropping the session drops the record of how it went with it — the two
    /// used to live in different trees under different rules, and a swept
    /// bucket left its logs behind for a fortnight.
    pub fn journal_path(&self, workspace: &Path, id: &str) -> PathBuf {
        self.dir_of(workspace)
            .join(tools::state::file_stem(id))
            .join("journal.jsonl")
    }

    /// The tree every bucket sits in, for the sweeps that walk all of them.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every bucket under the store root, each with its transcripts. The
    /// sweep and the removal walk the same tree; they differ in what they do
    /// with each bucket, not in how they find one.
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
            .join(tools::state::file_stem(id))
            .join("session.json")
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
        std::fs::create_dir_all(path.parent().expect("a session directory"))?;

        // Rename, so a crash mid-write cannot leave a truncated transcript.
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
        // Serialized by reference, straight into the file: a transcript is
        // megabytes by the end, and a `Stored` clone plus a `to_vec` buffer is
        // a second full copy that several parallel subagents each pay for.
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
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Load a transcript by id. `id` is unique across workspaces, so the
    /// bucket is found by searching the store's directories rather than by
    /// knowing which workspace saved it. Sessions saved before the bucketed
    /// layout sit flat under the root; the search falls back to that path.
    pub fn load(&self, id: &str) -> Result<Stored> {
        let stem = tools::state::file_stem(id);
        let mut match_: Option<PathBuf> = None;
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(&stem).join("session.json");
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

    /// Every session recorded for this workspace, newest first, read as
    /// shallowly as the answer allows. The legacy flat files under the root
    /// are read too: a session saved before the bucketed layout would
    /// otherwise vanish from `/resume` on upgrade.
    fn peek(&self, workspace: &Path) -> Vec<Peek> {
        let want = workspace.display().to_string();
        let mut found: Vec<Peek> = std::fs::read_dir(self.dir_of(workspace))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                // The bucket also holds `history`, which is a file and so is
                // skipped by asking for a transcript inside it.
                let path = entry.path().join("session.json");
                if !path.is_file() {
                    return None;
                }
                let body = std::fs::read_to_string(&path).ok()?;
                match serde_json::from_str::<Peek>(&body) {
                    Ok(peek) => (peek.workspace == want).then_some(peek),
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
        // Newest by last activity, not by creation. `/resume` is reached for
        // to pick up where you left off, and a session started last week and
        // worked on this morning is the one you mean.
        //
        // The id breaks a tie. It has to break somehow — a stamp is seconds,
        // and two sessions touched in the same one are common — and left to
        // `read_dir` the answer is whatever order the filesystem hands back,
        // which differs between machines and between runs on one.
        found.sort_by(|a, b| b.touched().cmp(&a.touched()).then_with(|| b.id.cmp(&a.id)));
        found
    }

    /// Sweep buckets nobody can reach. `/resume` lists one workspace's
    /// sessions, so a workspace that is gone has taken the only way back to
    /// them with it — a removed worktree, a deleted checkout.
    ///
    /// Reachability alone would not be safe: a path is also absent when a disk
    /// is not mounted this morning, and that must not cost a transcript. Age is
    /// what separates the two, so nothing recent goes whatever the path says.
    pub fn prune(&self) {
        self.prune_older_than(UNREACHED_KEEP);
    }

    /// The same, against a stated age rather than the constant — a test that
    /// waits a month is not a test.
    fn prune_older_than(&self, keep: std::time::Duration) {
        let now = std::time::SystemTime::now();
        for (bucket, transcripts) in self.buckets() {
            // Nothing here says which tree this bucket belongs to, so nothing
            // here can say it is gone. `remove_dir` takes the bucket only if it
            // is genuinely empty: one holding recall and no transcripts is
            // still somebody's, and is left alone.
            if transcripts.is_empty() {
                let _ = std::fs::remove_dir(&bucket);
                continue;
            }
            let recent = transcripts.iter().any(|p| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .is_none_or(|age| age < keep)
            });
            if recent {
                continue;
            }
            // One transcript answers for the bucket: they are grouped by the
            // very path being asked about.
            if workspace_of(&transcripts).is_some_and(|w| !Path::new(&w).is_dir()) {
                let _ = std::fs::remove_dir_all(&bucket);
            }
        }
    }
    /// Remove the records recorded under `root` — a checkout being removed
    /// takes its transcripts and journals with it now, rather than after the
    /// sweep's month of grace. Returns how many transcripts went, so the
    /// removal can say what it did.
    ///
    /// One session at a time rather than whole buckets: bucket names are a
    /// lossy encoding of the path (`key_of` folds a `/` and a `-` alike), so
    /// two trees can share one — deleting the bucket would take a live
    /// sibling tree's records with it. A bucket goes whole only when every
    /// transcript in it was recorded under `root`.
    pub fn drop_under(&self, root: &Path) -> usize {
        let mut dropped = 0;
        for (bucket, transcripts) in self.buckets() {
            // Each transcript names the workspace it was saved under; split
            // the bucket's sessions into ours and someone else's.
            let mut owed: Vec<&Path> = Vec::new();
            let mut shared = false;
            for transcript in &transcripts {
                let Ok(text) = std::fs::read_to_string(transcript) else {
                    continue;
                };
                let Ok(belongs) = serde_json::from_str::<Belongs>(&text) else {
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
                // memory and recall cannot be told apart; only ours go.
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
    /// by and its first prompt.
    pub fn choices(&self, workspace: &Path) -> Vec<ResumeChoice> {
        self.peek(workspace)
            .into_iter()
            .map(|p| ResumeChoice {
                prompt: p.opening().unwrap_or_default(),
                id: p.id,
                created: p.created,
            })
            .collect()
    }

    /// Most recent session recorded for this workspace, read in full — it is
    /// about to be resumed, which is the one time the whole transcript is
    /// wanted.
    pub fn latest(&self, workspace: &Path) -> Result<Stored> {
        let newest = self
            .peek(workspace)
            .into_iter()
            .next()
            .with_context(|| format!("no session recorded for {}", workspace.display()))?;
        self.load(&newest.id)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_ids_minted_in_one_second_are_still_two_ids() {
        // Same second, same pid: everything but the counter is equal.
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn an_id_cannot_walk_out_of_the_directory_it_names_a_file_in() {
        assert_eq!(
            tools::state::file_stem("../../etc/cron.d/x"),
            "______etc_cron_d_x"
        );
        assert_eq!(tools::state::file_stem(".."), "__");
    }

    use super::*;
    use brain::message::{AssistantContent, Message, ToolCall};
    use serde_json::json;

    fn log_with(messages: Vec<Message>) -> Session {
        Session::from_messages(messages)
    }

    /// Set what `touched()` reads — the last entry's stamp.
    ///
    /// A stamp is seconds, so archives saved in one second tie, and a tie is
    /// settled by the id rather than by recency. Forced through the JSON
    /// because an entry's stamp is the session's to set, not a caller's.
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
        Message::tool_results(vec![brain::message::ToolResult::text("c1", "read", "body")])
    }

    /// Unreachability is not enough on its own: a checkout that is merely
    /// unmounted looks exactly like one that was removed, and only the second
    /// is a reason to drop a transcript. Age is what tells them apart.
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
    /// `/worktree rm` drops a tree's buckets outright — transcripts, journals
    /// and recall — where the sweep only waits out the month of grace. A run
    /// started in a subdirectory of the tree belongs to it too, and a sibling
    /// tree's records must survive.
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
        let deep = tree.join("crates/cli");
        let sibling = home.path().join(".worktrees").join("feature-two");
        save("in-tree", &tree);
        save("in-deep", &deep);
        save("in-sibling", &sibling);

        assert_eq!(store.drop_under(&tree), 2);
        assert!(store.load("in-tree").is_err());
        assert!(store.load("in-deep").is_err());
        assert!(store.load("in-sibling").is_ok());
    }
    /// Two trees whose names fold to the same bucket key (`feature-x` and
    /// `feature/x`) share one directory on disk; removing one must leave the
    /// other's sessions alone.
    #[test]
    fn drop_under_spares_a_sibling_that_shares_a_bucket_key() {
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
        assert_eq!(tools::state::key_of(&tree), tools::state::key_of(&sibling));

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
        log.record(agent::session::Compaction {
            omissions: vec![agent::session::Omission {
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

        assert_eq!(store.latest(std::path::Path::new("/a")).unwrap().id, "new");
        assert_eq!(
            store.latest(std::path::Path::new("/b")).unwrap().id,
            "other"
        );
        assert!(store.latest(std::path::Path::new("/nowhere")).is_err());
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

        // Recency, decided on the field the sort reads. The expected order is
        // the reverse of the ids' own, so a list that fell back to breaking
        // ties by id would fail here rather than look right by accident.
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

    #[test]
    fn choices_show_the_first_question_and_skip_an_empty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let asked = log_with(vec![
            Message::user("why is the flaky test flaky?"),
            Message::assistant_text("there"),
        ]);
        store
            .save("a", std::path::Path::new("/w"), "m", None, 1, &asked)
            .unwrap();
        // A session that never got a prompt has nothing to resume to by name.
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
        let by = |id: &str| got.iter().find(|c| c.id == id).expect(id);
        assert_eq!(by("a").prompt, "why is the flaky test flaky?");
        assert_eq!(
            by("b").prompt,
            "",
            "nothing to name it by, and that is fine"
        );
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
