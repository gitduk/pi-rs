use std::path::{Path, PathBuf};

use agent::session::Session;
use anyhow::{Context, Result};
use brain::message::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Stored {
    pub id: String,
    pub workspace: String,
    pub model: String,
    pub created: u64,
    /// What the user calls this session. Ids are a timestamp and a pid, which
    /// nobody recognises a week later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten, default)]
    pub session: Session,
    /// Transcripts written before the log existed. Read, never written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    messages: Vec<Message>,
}

impl Stored {
    /// The transcript, whichever shape it was stored in.
    pub fn into_session(self) -> Session {
        if self.session.is_empty() && !self.messages.is_empty() {
            Session::from_messages(self.messages)
        } else {
            self.session
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

    pub fn save(
        &self,
        id: &str,
        workspace: &Path,
        model: &str,
        name: Option<&str>,
        session: &Session,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.path(id);

        let stored = Stored {
            id: id.to_string(),
            workspace: workspace.display().to_string(),
            model: model.to_string(),
            created: now(),
            name: name.map(str::to_string),
            session: session.clone(),
            messages: Vec::new(),
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

    /// Most recent session recorded for this workspace.
    pub fn latest(&self, workspace: &Path) -> Result<Stored> {
        let want = workspace.display().to_string();
        let mut best: Option<(u64, Stored)> = None;

        let entries = std::fs::read_dir(&self.root)
            .with_context(|| format!("no sessions yet under {}", self.root.display()))?;
        for entry in entries.flatten() {
            if entry.path().extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(stored) = serde_json::from_str::<Stored>(&body) else {
                continue;
            };
            if stored.workspace != want {
                continue;
            }
            if best.as_ref().is_none_or(|(t, _)| stored.created > *t) {
                best = Some((stored.created, stored));
            }
        }
        best.map(|(_, s)| s)
            .with_context(|| format!("no session recorded for {want}"))
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
    use brain::message::{AssistantContent, ToolCall, ToolCallId};
    use serde_json::json;

    fn log_with(messages: Vec<Message>) -> Session {
        Session::from_messages(messages)
    }

    fn call(name: &str) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: ToolCallId("c1".into()),
                provider: None,
                name: name.into(),
                args: json!({}),
            })],
        }
    }

    fn results() -> Message {
        Message::tool_results(vec![brain::message::ToolResult::text(
            ToolCallId("c1".into()),
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
            .save("t", std::path::Path::new("/w"), "m", None, &log)
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
        log.record(agent::session::Compaction {
            elisions: vec![agent::session::Elision {
                call: ToolCallId("c1".into()),
                notice: "[gone]".into(),
            }],
            ..Default::default()
        });
        store
            .save("t", std::path::Path::new("/w"), "m", None, &log)
            .unwrap();

        let back = store.load("t").unwrap().into_session();
        // The view is shrunk, and the body that was elided is still on disk.
        assert!(format!("{:?}", back.context()[2]).contains("[gone]"));
        assert!(
            back.messages()
                .any(|(_, m)| format!("{m:?}").contains("body"))
        );
    }

    #[test]
    fn a_transcript_written_before_the_log_existed_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            store.path("old"),
            r#"{"id":"old","workspace":"/w","model":"m","created":1,
                "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
        )
        .unwrap();

        let log = store.load("old").unwrap().into_session();
        assert_eq!(log.context().len(), 1);
        assert_eq!(log.context()[0].text(), "hi");
    }

    #[test]
    fn latest_picks_the_newest_session_for_that_workspace_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("x")]);
        store
            .save("old", std::path::Path::new("/a"), "m", None, &log)
            .unwrap();
        store
            .save("other", std::path::Path::new("/b"), "m", None, &log)
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
}
