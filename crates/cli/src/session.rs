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
    /// The transcript, whichever shape it was stored in.
    pub fn into_session(self) -> Session {
        if self.session.is_empty() && !self.messages.is_empty() {
            Session::from_messages(self.messages)
        } else {
            self.session
        }
    }
}
impl Stored {
    /// The first thing the user asked this session, if anything. What the
    /// resume list shows in place of the id.
    pub fn first_prompt(&self) -> Option<String> {
        if let Some(prompt) = self.session.first_prompt() {
            return Some(prompt);
        }
        self.messages
            .iter()
            .find_map(|m| matches!(m, Message::User { .. }).then(|| m.text()))
            .filter(|t| !t.is_empty())
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

    /// Every session recorded for this workspace, newest first. What `latest`
    /// returns is the top of this list.
    pub fn list(&self, workspace: &Path) -> Vec<Stored> {
        let want = workspace.display().to_string();
        let mut found: Vec<Stored> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                if entry.path().extension().is_none_or(|e| e != "json") {
                    return None;
                }
                let body = std::fs::read_to_string(entry.path()).ok()?;
                let stored = serde_json::from_str::<Stored>(&body).ok()?;
                (stored.workspace == want).then_some(stored)
            })
            .collect();
        found.sort_by_key(|s| std::cmp::Reverse(s.created));
        found
    }

    /// Every session `/resume` can name for this workspace, newest first,
    /// reduced to what the list and its completion show: the id it is named
    /// by and its first prompt.
    pub fn choices(&self, workspace: &Path) -> Vec<ResumeChoice> {
        self.list(workspace)
            .into_iter()
            .map(|s| {
                let prompt = s.first_prompt().unwrap_or_default();
                ResumeChoice {
                    id: s.id,
                    prompt,
                    created: s.created,
                }
            })
            .collect()
    }

    /// Most recent session recorded for this workspace: the top of `list`.
    pub fn latest(&self, workspace: &Path) -> Result<Stored> {
        self.list(workspace)
            .into_iter()
            .next()
            .with_context(|| format!("no session recorded for {}", workspace.display()))
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

    #[test]
    fn list_returns_this_workspaces_sessions_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let log = log_with(vec![Message::user("x")]);
        store
            .save("a", std::path::Path::new("/w"), "m", None, &log)
            .unwrap();
        store
            .save("b", std::path::Path::new("/w"), "m", None, &log)
            .unwrap();
        store
            .save("c", std::path::Path::new("/other"), "m", None, &log)
            .unwrap();

        // `created` has one-second resolution, so newness is forced explicitly.
        let mut stored = store.load("a").unwrap();
        stored.id = "newest".into();
        stored.created += 60;
        std::fs::write(store.path("newest"), serde_json::to_vec(&stored).unwrap()).unwrap();

        let ids: Vec<String> = store
            .list(std::path::Path::new("/w"))
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(ids, ["newest", "b", "a"]);
        // Another workspace's sessions stay out.
        assert!(
            store
                .list(std::path::Path::new("/other"))
                .iter()
                .all(|s| s.id == "c")
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
            .save("a", std::path::Path::new("/w"), "m", None, &asked)
            .unwrap();
        // A session that never got a prompt has nothing to resume to by name.
        store
            .save("b", std::path::Path::new("/w"), "m", None, &Session::new())
            .unwrap();

        let got = store.choices(std::path::Path::new("/w"));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "b"); // newest first, even when it has no prompt
        assert_eq!(got[0].prompt, "");
        assert_eq!(got[1].prompt, "why is the flaky test flaky?");
    }
}
