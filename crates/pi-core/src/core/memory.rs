//! Remembering without being asked: once pi has exited, a process of its own
//! distills what the sessions said into `pi_store::memory`, and the next run
//! carries what is remembered in its prompt.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent::session::Entry;
use llm::model::ModelSpec;
use llm::transport::Transport;
use pi_store::memory::{self, Memory};
use pi_store::session::{Progress, Store, Stored};

use super::worktree::main_root;

const PROMPT: &str = include_str!("../../prompts/distill.md");

const MAX_REPLY_TOKENS: u32 = 2_000;

// A run's worth of catching up; whatever is left waits for the next run.
const PER_RUN: usize = 10;

/// What the prompt may carry of memory, all files together. Past it the
/// rest is left out rather than the prompt growing without bound.
const BUDGET: usize = 16 * 1024;

/// The memory files a run in `dir` carries, by name, within the budget.
pub fn kept(memory: &Memory, dir: &Path) -> Vec<(String, String)> {
    let mut used = 0;
    let mut kept = Vec::new();
    for file in memory.files(&main_root(dir)) {
        if used + file.body.len() > BUDGET {
            tracing::warn!(target: "pi::memory", file = %file.name, "over the memory budget, left out");
            continue;
        }
        used += file.body.len();
        kept.push(file);
    }
    kept.into_iter().map(|f| (f.name, f.body)).collect()
}

/// Read what changed in the sessions since the last distillation and fold it
/// into memory; `ended` is the session whose exit asked for this. Nothing is
/// shown: what happened goes to the journal.
pub async fn distill(
    store: Store,
    memory: Memory,
    writer: (Arc<dyn Transport>, ModelSpec),
    idle: Duration,
    ended: &str,
) {
    let Some(_held) = memory.lock() else {
        return;
    };
    let mut marks = match memory.marks() {
        Some(marks) => marks,
        None => {
            // The first run remembers from the session that just ended on, and
            // what ran beside it, not the whole archive back.
            let all = store.progress();
            let since = all
                .iter()
                .find(|p| p.id == ended)
                .map_or(u64::MAX, |p| p.created);
            let seeded = all
                .into_iter()
                .filter(|p| p.touched < since)
                .map(|p| (p.id, p.last))
                .collect();
            save(&memory, &seeded);
            seeded
        }
    };

    let mut owed: Vec<Progress> = store
        .progress()
        .into_iter()
        .filter(|p| marks.get(&p.id).is_none_or(|&m| m < p.last))
        .collect();
    owed.sort_by_key(|p| p.touched);

    for p in owed.into_iter().take(PER_RUN) {
        let Ok(stored) = Store::read(&p.path) else {
            continue;
        };
        let from = marks.get(&p.id).copied();
        if !once(&memory, &stored, from, &writer, idle).await {
            continue;
        }
        marks.insert(p.id, p.last);
        save(&memory, &marks);
    }
}

// One session's new part. False when it should be tried again next run.
async fn once(
    memory: &Memory,
    stored: &Stored,
    from: Option<u64>,
    (transport, spec): &(Arc<dyn Transport>, ModelSpec),
    idle: Duration,
) -> bool {
    let entries: Vec<_> = stored
        .session
        .entries()
        .iter()
        .filter(|e| from.is_none_or(|f| e.id().0 > f))
        // Only the conversation: tool output is text from anywhere, and one
        // line of it remembered would ride every prompt after.
        .filter(|e| matches!(e, Entry::Ask { .. } | Entry::Answer { .. }))
        .collect();
    let history = agent::compaction::render(&[], &entries);
    if history.trim().is_empty() {
        return true;
    }
    let project = main_root(Path::new(&stored.workspace));
    let day = entries.first().map_or(stored.created, |e| e.at());
    let day = pi_store::journal::rfc3339(std::time::UNIX_EPOCH + Duration::from_secs(day));
    let body = format!(
        "Memory now:\n{}\n\nSession in {}, {}:\n{history}",
        match memory.files(&project) {
            files if files.is_empty() => "(nothing yet)".into(),
            files => agent::prompt::files(
                &files
                    .into_iter()
                    .map(|f| (f.name, f.body))
                    .collect::<Vec<_>>()
            ),
        },
        stored.workspace,
        day.split_once('T').map_or(day.as_str(), |(d, _)| d),
    );

    let reply =
        agent::compaction::ask(&**transport, spec, PROMPT, body, MAX_REPLY_TOKENS, idle).await;
    let (text, usage) = match reply {
        Ok(got) => got,
        Err(e) => {
            tracing::warn!(target: "pi::memory", session = %stored.id, error = %e, "distilling failed");
            return false;
        }
    };
    // A reply that cannot be read would be one forever; it is passed over.
    let edits = match memory::parse(&text) {
        Ok(edits) => edits,
        Err(why) => {
            tracing::warn!(target: "pi::memory", session = %stored.id, %why, "unreadable reply");
            return true;
        }
    };
    match memory.apply(&project, &edits) {
        Ok(changed) => {
            tracing::info!(
                target: "pi::memory",
                session = %stored.id,
                changed,
                input = usage.input,
                output = usage.output,
                "distilled"
            );
            true
        }
        Err(e) => {
            tracing::warn!(target: "pi::memory", session = %stored.id, error = %e, "memory not written");
            false
        }
    }
}

fn save(memory: &Memory, marks: &BTreeMap<String, u64>) {
    if let Err(e) = memory.save_marks(marks) {
        tracing::warn!(target: "pi::memory", error = %e, "distillation marks not saved");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::stream::{BlockKind, StopReason, StreamEvent, Usage};
    use std::sync::Mutex;

    // Answers every request with `reply`, keeping what it was asked.
    struct Scripted {
        reply: String,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Transport for Scripted {
        async fn stream(
            &self,
            _spec: &ModelSpec,
            req: &llm::request::Request,
        ) -> llm::Result<futures::stream::BoxStream<'static, llm::Result<StreamEvent>>> {
            let asked = serde_json::to_string(&req.messages).unwrap_or_default();
            self.asked.lock().unwrap().push(asked);
            let events = vec![
                Ok(StreamEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                }),
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    delta: self.reply.clone(),
                }),
                Ok(StreamEvent::Done {
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    // The marks are what keep a session from being read twice, or never:
    // nobody would see either go wrong.
    #[tokio::test]
    async fn each_part_of_a_session_is_read_once_and_the_archive_before_never() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        let memory = Memory::new(dir.path().join("memory"));
        let ws = dir.path().join("work");
        std::fs::create_dir_all(&ws).unwrap();
        let mut session = agent::session::Session::default();
        session.send_prompt("from before memory existed", None);
        store.save("s1", &ws, "m", None, 0, &session).unwrap();

        let wire = Arc::new(Scripted {
            reply: r#"[{"file": "user.md", "add": "writes comments in English"}]"#.into(),
            asked: Mutex::default(),
        });
        let writer = (
            wire.clone() as Arc<dyn Transport>,
            crate::core::tests::test_spec("m"),
        );
        let idle = Duration::from_secs(5);

        // First run: everything already on disk counts as read, but for the
        // session whose exit asked.
        distill(store.clone(), memory.clone(), writer.clone(), idle, "other").await;
        assert!(wire.asked.lock().unwrap().is_empty());

        session.send_prompt("comments in English, please", None);
        store.save("s1", &ws, "m", None, 0, &session).unwrap();
        distill(store.clone(), memory.clone(), writer.clone(), idle, "s1").await;
        let asked = wire.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].contains("comments in English"), "{}", asked[0]);
        assert!(!asked[0].contains("from before memory"), "{}", asked[0]);
        let files = memory.files(&ws);
        assert_eq!(files[0].body, "- writes comments in English\n");

        // Nothing new since: nothing asked.
        distill(store.clone(), memory, writer.clone(), idle, "s1").await;
        assert_eq!(wire.asked.lock().unwrap().len(), 1);

        let fresh = Memory::new(dir.path().join("fresh"));
        distill(store, fresh, writer, idle, "s1").await;
        let asked = wire.asked.lock().unwrap().clone();
        assert!(asked[1].contains("from before memory"), "{}", asked[1]);
    }
}
