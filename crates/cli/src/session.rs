use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use brain::message::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Stored {
    pub id: String,
    pub workspace: String,
    pub model: String,
    pub created: u64,
    pub messages: Vec<Message>,
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
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self::new(base.join("pir/sessions"))
    }
}

pub fn new_id() -> String {
    format!("{}-{}", now(), std::process::id())
}

/// Graft a new prompt onto a saved transcript.
///
/// A run that died mid-turn usually ends on tool results the model never
/// answered. Trimming back to the last clean assistant turn would throw the
/// whole conversation away, so the prompt joins that trailing user message
/// instead — results and text in one turn is a shape both wires accept. Only an
/// assistant turn whose calls were never answered has to go: an unanswered
/// `tool_use` makes the next request invalid.
pub fn resume_with(mut messages: Vec<Message>, prompt: String) -> Vec<Message> {
    while messages
        .last()
        .is_some_and(|m| matches!(m, Message::Assistant { .. }) && m.tool_calls().next().is_some())
    {
        messages.pop();
    }

    match messages.last_mut() {
        Some(Message::User { content }) => {
            content.push(brain::message::UserContent::Text(brain::message::Text {
                text: prompt,
            }));
        }
        _ => messages.push(Message::user(prompt)),
    }
    messages
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    pub fn save(
        &self,
        id: &str,
        workspace: &Path,
        model: &str,
        messages: &[Message],
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.path(id);

        let stored = Stored {
            id: id.to_string(),
            workspace: workspace.display().to_string(),
            model: model.to_string(),
            created: now(),
            messages: messages.to_vec(),
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
    use super::*;
    use brain::message::{AssistantContent, ToolCall, ToolCallId};
    use serde_json::json;

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
    fn an_unanswered_call_is_dropped_and_the_prompt_joins_the_turn_before_it() {
        let messages = vec![
            Message::user("hi"),
            Message::assistant_text("ok"),
            call("read"),
        ];
        let out = resume_with(messages, "next".into());

        // The dangling call is gone; the two turns before it are not, and the
        // prompt starts a fresh turn because the last one is a clean answer.
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].text(), "ok");
        assert_eq!(out[2].text(), "next");
        assert!(out.iter().all(|m| m.tool_calls().next().is_none()));
    }

    #[test]
    fn a_prompt_after_tool_results_joins_them_rather_than_starting_a_turn() {
        let messages = vec![Message::user("hi"), call("read"), results()];
        let out = resume_with(messages, "and now?".into());

        // Two user turns in a row are rejected outright, and trimming back to
        // the last clean assistant turn would discard the whole conversation.
        assert_eq!(out.len(), 3);
        let Message::User { content } = &out[2] else {
            panic!()
        };
        assert_eq!(content.len(), 2);
        assert!(matches!(
            &content[0],
            brain::message::UserContent::ToolResult(_)
        ));
        assert_eq!(out[2].text(), "and now?");
    }

    #[test]
    fn a_clean_transcript_gains_one_new_turn() {
        let messages = vec![Message::user("hi"), Message::assistant_text("done")];
        let out = resume_with(messages, "more".into());
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].text(), "more");
    }

    #[test]
    fn an_empty_transcript_starts_the_conversation() {
        let out = resume_with(Vec::new(), "first".into());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text(), "first");
    }

    #[test]
    fn a_transcript_with_tool_traffic_survives_the_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let messages = vec![Message::user("go"), call("read"), results()];
        store
            .save("t", std::path::Path::new("/w"), "m", &messages)
            .unwrap();

        let back = store.load("t").unwrap();
        assert_eq!(
            back.messages.len(),
            3,
            "tool traffic must not vanish in transit"
        );
        assert_eq!(back.messages[1].tool_calls().next().unwrap().name, "read");
        assert_eq!(
            store
                .latest(std::path::Path::new("/w"))
                .unwrap()
                .messages
                .len(),
            3
        );
    }

    #[test]
    fn a_saved_transcript_round_trips_and_stays_private() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let messages = vec![Message::user("hi"), Message::assistant_text("there")];
        let path = store
            .save("t1", std::path::Path::new("/w"), "opus-5", &messages)
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
        let back = store.load("t1").unwrap();
        assert_eq!(back.model, "opus-5");
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.messages[1].text(), "there");
    }

    #[test]
    fn latest_picks_the_newest_session_for_that_workspace_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let msg = vec![Message::user("x")];
        store
            .save("old", std::path::Path::new("/a"), "m", &msg)
            .unwrap();
        store
            .save("other", std::path::Path::new("/b"), "m", &msg)
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
