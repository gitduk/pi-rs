use std::path::{Path, PathBuf};

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

/// An archive read only as far as the listing needs.
///
/// The identity fields are required, exactly as `Stored` requires them, so a
/// session that lists is a session that loads. Everything under them is
/// optional and shallow: `serde` skips a field no struct here names without
/// building it, and what it skips is the whole of the transcript.
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

pub fn new_id() -> String {
    format!("{}-{}", now(), std::process::id())
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.json", tools::state::file_stem(id)))
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
        std::fs::create_dir_all(&self.root)?;
        let path = self.path(id);

        let stored = Stored {
            id: id.to_string(),
            workspace: workspace.display().to_string(),
            model: model.to_string(),
            created,
            name: name.map(str::to_string),
            session: session.clone(),
        };

        // Rename, so a crash mid-write cannot leave a truncated transcript.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&stored)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The transcript holds prompts and file contents.
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn load(&self, id: &str) -> Result<Stored> {
        let path = self.path(id);
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("no session `{id}` at {}", path.display()))?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Every session recorded for this workspace, newest first, read as
    /// shallowly as the answer allows.
    ///
    /// A listing needs four fields and one sentence. Reading each archive as a
    /// whole `Stored` to get them meant deserializing every reasoning block and
    /// every tool result in every session — 44 MB on one developer's machine,
    /// and it grows every day pi is used. `Peek` takes the same identity fields
    /// `load` requires, so what the list accepts is exactly what can be
    /// resumed; the entries below them are read shallowly, because their shape
    /// is not the list's business.
    fn peek(&self, workspace: &Path) -> Vec<Peek> {
        let want = workspace.display().to_string();
        let mut found: Vec<Peek> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                if entry.path().extension().is_none_or(|e| e != "json") {
                    return None;
                }
                let body = std::fs::read_to_string(entry.path()).ok()?;
                // Said rather than skipped. A transcript this build cannot read
                // is still on disk, and swallowing it makes "unreadable" look
                // exactly like "you have no sessions".
                match serde_json::from_str::<Peek>(&body) {
                    Ok(peek) => (peek.workspace == want).then_some(peek),
                    Err(e) => {
                        tracing::warn!(
                            target: "pi::session",
                            path = %entry.path().display(),
                            error = %e,
                            "unreadable transcript skipped"
                        );
                        None
                    }
                }
            })
            .collect();
        // Newest by last activity, not by creation. `/resume` is reached for
        // to pick up where you left off, and a session started last week and
        // worked on this morning is the one you mean.
        found.sort_by_key(|p| std::cmp::Reverse(p.touched()));
        found
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
    fn an_id_cannot_walk_out_of_the_directory_it_names_a_file_in() {
        assert_eq!(tools::state::file_stem("../../etc/cron.d/x"), "______etc_cron_d_x");
        assert_eq!(tools::state::file_stem(".."), "__");
    }

    use super::*;
    use brain::message::{AssistantContent, Message, ToolCall};
    use serde_json::json;

    fn log_with(messages: Vec<Message>) -> Session {
        Session::from_messages(messages)
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
        Message::tool_results(vec![brain::message::ToolResult::text(
            "c1",
            "read",
            "body",
        )])
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
        assert!(flat.contains_key("entries"), "session must flatten to the top level");
        assert!(!flat.contains_key("log"), "the nested `log` key must be gone");
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

        // `created` has one-second resolution, so newness is forced explicitly.
        let mut stored = store.load("old").unwrap();
        stored.id = "new".into();
        stored.created += 60;
        std::fs::write(store.path("new"), serde_json::to_vec(&stored).unwrap()).unwrap();

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

        // `created` has one-second resolution, so newness is forced explicitly.
        let mut stored = store.load("a").unwrap();
        stored.id = "newest".into();
        stored.created += 60;
        std::fs::write(store.path("newest"), serde_json::to_vec(&stored).unwrap()).unwrap();

        let ids: Vec<String> = store
            .choices(std::path::Path::new("/w"))
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(ids, ["newest", "b", "a"]);
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
            .save("b", std::path::Path::new("/w"), "m", None, 1, &Session::new())
            .unwrap();

        let got = store.choices(std::path::Path::new("/w"));
        assert_eq!(got.len(), 2, "a session with no prompt is still resumable");
        let by = |id: &str| got.iter().find(|c| c.id == id).expect(id);
        assert_eq!(by("a").prompt, "why is the flaky test flaky?");
        assert_eq!(by("b").prompt, "", "nothing to name it by, and that is fine");
    }
}
