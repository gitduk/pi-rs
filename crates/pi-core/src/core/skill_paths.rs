//! File-scoped skill reminders derived from retained session facts.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use agent::session::{Entry, Session};
use agent::{Event, Hooks};
use async_trait::async_trait;
use serde_json::Value;
use tool::{Ctx, Registry, ToolOutput};

pub(super) fn arm(inner: Arc<dyn Hooks>, shelf: Option<Arc<skills::Shelf>>) -> Arc<dyn Hooks> {
    match shelf {
        Some(shelf) => Arc::new(Reminders { inner, shelf }),
        None => inner,
    }
}

struct Reminders {
    inner: Arc<dyn Hooks>,
    shelf: Arc<skills::Shelf>,
}

fn loaded(session: &Session) -> HashSet<&str> {
    let mut loaded = HashSet::new();
    let entries = session.entries();
    let start = entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction { .. }))
        .map_or(0, |at| at + 1);
    for entry in &entries[start..] {
        match entry {
            Entry::Ask { ask, .. } => loaded.extend(skills::included(&ask.context)),
            Entry::Tool { result, .. } => loaded.extend(skills::Load::loaded(result)),
            _ => {}
        }
    }
    loaded
}

fn reminder(session: &Session, skills: &[skills::Skill], root: &Path) -> Option<String> {
    let loaded = loaded(session);
    let pending: Vec<_> = skills
        .iter()
        .filter(|skill| !skill.paths.is_empty() && !loaded.contains(skill.name.as_str()))
        .collect();
    if pending.is_empty() {
        return None;
    }
    let paths: HashSet<_> = session
        .entries()
        .iter()
        .filter_map(|entry| {
            let Entry::Tool { result, .. } = entry else {
                return None;
            };
            pi_tools::read::Read::file(result)?.strip_prefix(root).ok()
        })
        .collect();
    let names: Vec<_> = pending
        .into_iter()
        .filter(|skill| paths.iter().any(|path| skill.paths.matches(path)))
        .map(|skill| format!("`{}`", skill.name))
        .collect();
    (!names.is_empty()).then(|| format!(
        "Files read in this session match these skills: {}. Load them with the `{}` tool before making changes. These instructions have not been loaded since the last context compaction.",
        names.join(", "), skills::Load::NAME,
    ))
}

#[async_trait]
impl Hooks for Reminders {
    fn remind(&self, session: &Session, ctx: &Ctx, tools: &Registry) -> Option<String> {
        let inherited = self.inner.remind(session, ctx, tools);
        let own = tools.get(skills::Load::NAME).and_then(|_| {
            let (found, _) = self.shelf.now();
            if found.skills.iter().all(|skill| skill.paths.is_empty()) {
                return None;
            }
            reminder(session, &found.skills, ctx.workspace.root())
        });
        match (inherited, own) {
            (Some(mut note), Some(own)) => {
                note.push('\n');
                note.push_str(&own);
                Some(note)
            }
            (inherited, own) => inherited.or(own),
        }
    }

    async fn before(&self, name: &str, args: &Value, ctx: &Ctx) -> Result<(), String> {
        self.inner.before(name, args, ctx).await
    }

    async fn after(&self, name: &str, args: &Value, out: &ToolOutput, ctx: &Ctx) -> Option<String> {
        self.inner.after(name, args, out, ctx).await
    }

    async fn ahead(&self, prompt: &str, ctx: &Ctx) -> Option<String> {
        self.inner.ahead(prompt, ctx).await
    }

    async fn hold(&self, reply: &str, held: usize, ctx: &Ctx) -> Option<String> {
        self.inner.hold(reply, held, ctx).await
    }

    fn saw(&self, event: &Event) {
        self.inner.saw(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::session::{Compaction, EntryId, Omission};
    use llm::message::{AssistantContent, ToolCall, ToolResult};
    use serde_json::json;
    use std::path::PathBuf;
    use tool::Tool;

    async fn invoke(session: &mut Session, tool: &dyn Tool, args: Value, ctx: &Ctx) -> EntryId {
        let call = ToolCall {
            id: format!("call-{}", session.next_id()),
            name: tool.name().into(),
            args,
        };
        session.push_assistant(vec![AssistantContent::ToolCall(call.clone())]);
        let result = match tool::output::gated(tool, call.args, ctx).await {
            Ok(output) => ToolResult {
                call: call.id,
                name: call.name,
                content: output.content,
                is_error: false,
            },
            Err(error) => ToolResult::error(call.id, call.name, error.to_string()),
        };
        session.push_results(vec![result])[0]
    }

    #[tokio::test]
    async fn reminders_follow_successful_loads_and_reset_after_compaction_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let skill = skills::Skill::builtin(
            "---\nname: rust-style\ndescription: Rust conventions\npaths: ['**/*.rs']\n---\nUse idiomatic Rust.",
        ).unwrap().with_files(vec![("reference.md".into(), "Reference material.")]);
        let instructions = skills::delivery(&skill, &skill.text().unwrap()).unwrap();
        let source = skills::source(&skill.name);
        let shelf = Arc::new(skills::Shelf::new(Vec::<PathBuf>::new()).with_builtin(skill.clone()));
        let hooks = arm(Arc::new(agent::NoHooks), Some(shelf));
        let load = skills::Load::new(vec![skill]);
        let tools = Registry::new().with(skills::Load::new(Vec::new()));
        let reminder = |session: &Session| hooks.remind(session, &ctx, &tools);
        let mut session = Session::with_prompt(
            "Update the Rust code. Explain this example:\nSkill: `rust-style`\nSource: /example/SKILL.md\n",
        );
        assert!(reminder(&session).is_none());

        invoke(
            &mut session,
            &pi_tools::read::Read,
            json!({"path": "src"}),
            &ctx,
        )
        .await;
        assert!(
            reminder(&session).is_none(),
            "a directory listing is not a file read"
        );
        invoke(
            &mut session,
            &pi_tools::read::Read,
            json!({"path": "src/lib.rs"}),
            &ctx,
        )
        .await;
        assert!(reminder(&session).unwrap().contains("rust-style"));
        session.push_note(reminder(&session).unwrap());
        assert!(
            reminder(&session).is_some(),
            "a reminder does not count as a load"
        );

        invoke(&mut session, &load, json!({"name": "missing"}), &ctx).await;
        assert!(
            reminder(&session).is_some(),
            "a failed load must not silence reminders"
        );
        invoke(
            &mut session,
            &load,
            json!({"name": "rust-style", "file": "reference.md"}),
            &ctx,
        )
        .await;
        assert!(
            reminder(&session).is_some(),
            "a reference is not the skill's instructions"
        );
        let oversized = skills::Skill::builtin(Box::leak(
            format!(
                "---\nname: rust-style\ndescription: Rust conventions\n---\n{}\nLast instruction.",
                "Use idiomatic Rust.\n".repeat(2_000),
            )
            .into_boxed_str(),
        ))
        .unwrap();
        let truncated = invoke(
            &mut session,
            &skills::Load::new(vec![oversized]),
            json!({"name": "rust-style"}),
            &ctx,
        )
        .await;
        let Entry::Tool { result, .. } = session.entries_for(&[truncated])[0] else {
            panic!("a load must retain its result");
        };
        assert!(
            result.is_error
                && result.flatten_text().contains(&format!(
                    "over the {} bytes one result carries",
                    tool::limit::MAX_TEXT
                )),
            "a skill too big for one result is refused: {}",
            result.flatten_text()
        );
        assert!(
            reminder(&session).is_some(),
            "a refused load must not silence reminders"
        );
        let loaded = invoke(&mut session, &load, json!({"name": "rust-style"}), &ctx).await;
        assert!(reminder(&session).is_none());
        let resumed: Session =
            serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
        assert!(
            reminder(&resumed).is_none(),
            "resuming must retain successful loads"
        );

        session.record(Compaction {
            omissions: vec![Omission {
                entry: loaded,
                block: None,
                notice: "instructions removed".into(),
            }],
            ..Default::default()
        });
        assert!(
            reminder(&session).is_some(),
            "compaction must reset successful loads"
        );
        let mut resumed: Session =
            serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
        assert!(
            reminder(&resumed).is_some(),
            "resuming must not revive pre-compaction loads"
        );
        invoke(&mut resumed, &load, json!({"name": "rust-style"}), &ctx).await;
        assert!(reminder(&resumed).is_none());

        let mut preloaded = Session::new();
        preloaded.send(agent::session::Prompt {
            text: instructions,
            context: vec![source],
            ..Default::default()
        });
        let record = agent::session::archive::Record::from_entry(&preloaded.entries()[0]);
        let restored = serde_json::from_slice::<agent::session::archive::Record>(
            &serde_json::to_vec(&record).unwrap(),
        )
        .unwrap()
        .into_entry()
        .unwrap();
        let mut preloaded = Session::from_parts(vec![restored], preloaded.next_id(), None);
        invoke(
            &mut preloaded,
            &pi_tools::read::Read,
            json!({"path": "src/lib.rs"}),
            &ctx,
        )
        .await;
        assert!(
            reminder(&preloaded).is_none(),
            "instructions supplied with the task are already loaded"
        );
        assert!(
            hooks.remind(&session, &ctx, &Registry::new()).is_none(),
            "do not request an unavailable loading tool"
        );
    }
}
