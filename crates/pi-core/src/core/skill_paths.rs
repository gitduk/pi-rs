//! Attach skill instructions when a tool batch reads matching files.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use agent::session::{Context, Entry, Seen, Session};
use agent::{ContextRequest, Decision, Event, Hooks};
use async_trait::async_trait;
use llm::estimate;
use serde_json::{Value, json};
use skills::{Load, Shelf};
use tool::{Ctx, ToolOutput};

pub(super) fn arm(inner: Arc<dyn Hooks>, shelf: Option<Arc<Shelf>>) -> Arc<dyn Hooks> {
    match shelf {
        Some(shelf) => Arc::new(PathSkills { inner, shelf }),
        None => inner,
    }
}

struct PathSkills {
    inner: Arc<dyn Hooks>,
    shelf: Arc<Shelf>,
}

fn recent_reads<'a>(recent: &'a [Entry], root: &Path) -> HashSet<&'a Path> {
    recent
        .iter()
        .filter_map(|entry| {
            let Entry::Tool { result, .. } = entry else {
                return None;
            };
            pi_tools::read::Read::file(result)?.strip_prefix(root).ok()
        })
        .collect()
}

fn visible_skills(session: &Session) -> HashSet<&str> {
    let mut names = HashSet::new();
    let mut calls = HashMap::new();
    for seen in session.view() {
        let Seen::As(entry) = seen else { continue };
        match entry {
            Entry::Ask { ask, .. } => names.extend(skills::included(&ask.context)),
            Entry::Context { context, .. } => {
                names.extend(skills::source_name(&context.source));
            }
            Entry::Answer { .. } => {
                calls.clear();
                calls.extend(entry.tool_calls().filter_map(|call| {
                    Load::instruction_name(call).map(|name| (call.id.as_str(), name))
                }));
            }
            Entry::Tool { result, .. } if result.name == Load::NAME => {
                names.extend(
                    calls
                        .remove(result.call.as_str())
                        .filter(|_| !result.is_error),
                );
            }
            _ => {}
        }
    }
    names
}

#[async_trait]
impl Hooks for PathSkills {
    async fn context(&self, request: ContextRequest<'_>) -> Vec<Context> {
        let mut attachments = self.inner.context(request).await;
        let paths = recent_reads(request.recent, request.ctx.workspace.root());
        let mut room = request.budget.saturating_sub(
            attachments
                .iter()
                .map(|entry| attachment_tokens(&entry.text))
                .sum(),
        );
        if paths.is_empty() || room == 0 {
            return attachments;
        }
        let shelf = self.shelf.clone();
        let tools = request.tools.clone();
        let Ok(Some((tool, found))) = tokio::task::spawn_blocking(move || {
            let tool = tools.get(Load::NAME)?;
            Some((tool, shelf.now().0))
        })
        .await
        else {
            return attachments;
        };
        let matching: Vec<_> = found
            .skills
            .iter()
            .filter(|skill| paths.iter().any(|path| skill.paths.matches(path)))
            .collect();
        if matching.is_empty() {
            return attachments;
        }
        let visible = visible_skills(request.session);
        for skill in matching {
            if room == 0 {
                break;
            }
            if visible.contains(skill.name.as_str()) {
                continue;
            }
            let args = json!({ "name": skill.name });
            if !matches!(
                request.approver.approve(Load::NAME, tool.tier(), &args),
                Decision::Allow
            ) || self.before(Load::NAME, &args, request.ctx).await.is_err()
            {
                continue;
            }
            match skill.read_instructions().await {
                Ok(text) => {
                    let bytes = text.len();
                    let text = format!(
                        "Automatically attached skill instructions for the current task:\n\n{text}"
                    );
                    let Some(left) = room.checked_sub(attachment_tokens(&text)) else {
                        continue;
                    };
                    room = left;
                    skill.record_delivery(bytes).await;
                    attachments.push(Context {
                        source: skills::source(&skill.name),
                        text,
                    });
                }
                Err(error) => {
                    tracing::warn!(target: "pi::skills", skill = %skill.name, %error, "could not attach skill instructions")
                }
            }
        }
        attachments
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

fn attachment_tokens(text: &str) -> usize {
    estimate::MESSAGE_OVERHEAD + estimate::text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::session::{Compaction, Omission};
    use llm::message::{AssistantContent, ToolCall, ToolResult};
    use pi_store::session::{Header, Store};
    use std::path::PathBuf;
    use tool::{Registry, Tier, Tool};

    struct Reject;

    impl agent::Approver for Reject {
        fn approve(&self, _: &str, _: Tier, _: &Value) -> Decision {
            Decision::Deny("denied by policy".into())
        }
    }

    #[async_trait]
    impl Hooks for Reject {
        async fn before(&self, name: &str, args: &Value, _: &Ctx) -> Result<(), String> {
            assert_eq!(name, Load::NAME);
            assert_eq!(*args, json!({"name": "rust-style"}));
            Err("denied by hook".into())
        }
    }

    fn request<'a>(session: &'a Session, ctx: &'a Ctx, tools: &'a Registry) -> ContextRequest<'a> {
        let start = session
            .entries()
            .iter()
            .rposition(|entry| matches!(entry, Entry::Answer { .. } | Entry::Compaction { .. }))
            .map_or(0, |at| at + 1);
        ContextRequest {
            session,
            recent: &session.entries()[start..],
            ctx,
            tools,
            approver: &agent::Ceiling(Tier::Read),
            budget: 2_000,
        }
    }

    async fn batch(session: &mut Session, calls: &[(&dyn Tool, Value)], ctx: &Ctx) {
        let calls: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(i, (tool, args))| {
                (
                    *tool,
                    ToolCall {
                        id: format!("call-{i}"),
                        name: tool.name().into(),
                        args: args.clone(),
                    },
                )
            })
            .collect();
        session.push_assistant(
            calls
                .iter()
                .map(|(_, call)| AssistantContent::ToolCall(call.clone()))
                .collect(),
        );
        let mut results = Vec::new();
        for (tool, call) in calls {
            results.push(match tool::output::gated(tool, call.args, ctx).await {
                Ok(out) => ToolResult {
                    call: call.id,
                    name: call.name,
                    content: out.content,
                    is_error: false,
                },
                Err(error) => ToolResult::error(call.id, call.name, error.to_string()),
            });
        }
        session.push_results(results);
    }

    #[tokio::test]
    async fn a_read_attaches_instructions_only_when_they_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn main() {}").unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let skill = skills::Skill::builtin(
            "---\nname: rust-style\ndescription: Rust conventions\npaths: ['**/*.rs']\n---\nUse idiomatic Rust.",
        ).unwrap().with_files(vec![("reference.md".into(), "Reference material.")]);
        let shelf = Arc::new(Shelf::new(Vec::<PathBuf>::new()).with_builtin(skill.clone()));
        let hooks = arm(Arc::new(agent::NoHooks), Some(shelf.clone()));
        let load = Load::new(vec![skill.clone()]);
        let tools = Registry::new().with(Load::new(vec![skill]));
        let read: (&dyn Tool, _) = (&pi_tools::read::Read, json!({"path": "lib.rs"}));

        let mut session = Session::with_prompt("Update the Rust code.");
        batch(
            &mut session,
            &[(&pi_tools::read::Read, json!({"path": "."}))],
            &ctx,
        )
        .await;
        assert!(
            hooks
                .context(request(&session, &ctx, &tools))
                .await
                .is_empty()
        );
        batch(
            &mut session,
            &[
                read.clone(),
                (&load, json!({"name": "rust-style", "file": "reference.md"})),
            ],
            &ctx,
        )
        .await;
        let mut attachments = hooks.context(request(&session, &ctx, &tools)).await;
        assert_eq!(attachments.len(), 1);
        let attachment = attachments.pop().unwrap();
        assert!(attachment.text.contains("Use idiomatic Rust."));
        let id = session.push_context(attachment);
        let store = Store::new(dir.path().join("sessions"));
        let location = store
            .save(
                dir.path(),
                Header {
                    id: "path-skills",
                    model: "test",
                    name: None,
                    created: 0,
                    spent: Default::default(),
                },
                &session,
            )
            .unwrap();
        let mut resumed = Store::read(&location).unwrap().into_session();
        assert_eq!(resumed.context(), session.context());

        batch(&mut resumed, std::slice::from_ref(&read), &ctx).await;
        assert!(
            hooks
                .context(request(&resumed, &ctx, &tools))
                .await
                .is_empty(),
            "the attachment survived resume"
        );
        let before_fit = resumed.entries().len() - 1;
        resumed.record(Compaction {
            omissions: vec![Omission {
                entry: id,
                block: None,
                notice: "instructions removed".into(),
            }],
            ..Default::default()
        });
        assert!(
            hooks
                .context(request(&resumed, &ctx, &tools))
                .await
                .is_empty(),
            "compaction does not trigger a load"
        );
        let mut fitted = request(&resumed, &ctx, &tools);
        fitted.recent = &resumed.entries()[before_fit..before_fit + 1];
        assert_eq!(
            hooks.context(fitted).await.len(),
            1,
            "a read that triggered the current request survives fitting"
        );
        assert!(
            hooks
                .context(ContextRequest {
                    budget: 1,
                    ..fitted
                })
                .await
                .is_empty()
        );
        assert!(
            hooks
                .context(ContextRequest {
                    approver: &Reject,
                    ..fitted
                })
                .await
                .is_empty()
        );
        assert!(
            arm(Arc::new(Reject), Some(shelf))
                .context(fitted)
                .await
                .is_empty()
        );
        batch(&mut resumed, std::slice::from_ref(&read), &ctx).await;
        assert_eq!(
            hooks.context(request(&resumed, &ctx, &tools)).await.len(),
            1
        );

        batch(
            &mut resumed,
            &[read.clone(), (&load, json!({"name": "rust-style"}))],
            &ctx,
        )
        .await;
        assert!(
            hooks
                .context(request(&resumed, &ctx, &tools))
                .await
                .is_empty(),
            "a successful manual load in the same batch suffices"
        );
        let manual_body = resumed.entries().last().unwrap().id();
        resumed.record(Compaction {
            omissions: vec![Omission {
                entry: manual_body,
                block: None,
                notice: "body removed".into(),
            }],
            ..Default::default()
        });
        batch(
            &mut resumed,
            &[
                read,
                (&load, json!({"name": "rust-style", "file": "reference.md"})),
            ],
            &ctx,
        )
        .await;
        assert_eq!(
            hooks.context(request(&resumed, &ctx, &tools)).await.len(),
            1,
            "a later reference result with the same call id is not the omitted body"
        );
    }
}
