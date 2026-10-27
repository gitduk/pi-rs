//! Shell jobs owned by a single run rather than an interactive session.

use std::sync::Arc;

use tool::{Ctx, Registry};

use crate::driver::jobs::{Jobs, Table};

/// Give a run a shell and job controls of its own. Its cancellation releases
/// unfinished commands; another run cannot list or stop them.
pub fn scoped(ctx: &Ctx, mut registry: Registry) -> Registry {
    let table = Arc::new(Table::default());
    let jobs = Arc::new(Jobs::scoped(table.clone()));
    registry.fallback(Arc::new(
        toolbox::bash::Bash::scoped(jobs.clone()).with_controls(jobs),
    ));

    let stop = ctx.cancel.clone();
    let root = ctx.workspace.root().to_path_buf();
    tokio::spawn(async move {
        stop.cancelled().await;
        table.stop_running(&root, "the owning run ended");
    });
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use llm::message::{Message, UserContent};
    use llm::stream::{BlockKind, StopReason, StreamEvent};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn scoped_shell_registration_respects_tool_cuts_and_live_sources() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let _stop = ctx.cancel.clone().drop_guard();
        let full = scoped(&ctx, toolbox::builtin());
        assert!(full.get("job").is_none());
        let shell = full.get(toolbox::bash::Bash::NAME).unwrap();
        assert!(shell.schema()["properties"].get("join").is_some());
        assert!(shell.schema()["properties"].get("wait").is_none());
        let read_only = scoped(&ctx, toolbox::builtin().within(tool::Tier::Read));
        assert!(read_only.get(toolbox::bash::Bash::NAME).is_none());
        let hidden = scoped(&ctx, toolbox::builtin().without(toolbox::bash::Bash::NAME));
        assert!(hidden.get(toolbox::bash::Bash::NAME).is_none());

        let script = dir.path().join("bash");
        std::fs::write(
            &script,
            "#!/bin/sh\n# ---\n# description = \"custom shell\"\n# ---\nprintf custom\n",
        )
        .unwrap();
        let mut registry = toolbox::builtin();
        registry.read(Arc::new(toolbox::scripts::Dir::new(dir.path())));
        let registry = scoped(&ctx, registry);
        let custom = registry.get(toolbox::bash::Bash::NAME).unwrap();
        assert!(
            custom
                .execute(json!({}), &ctx)
                .await
                .unwrap()
                .flatten()
                .contains("custom")
        );
        std::fs::remove_file(script).unwrap();
        let builtin = registry.get(toolbox::bash::Bash::NAME).unwrap();
        assert!(
            builtin
                .execute(json!({"command": "echo builtin"}), &ctx)
                .await
                .unwrap()
                .flatten()
                .contains("builtin")
        );
    }

    struct ShellModel {
        step: AtomicUsize,
        root: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl llm::Transport for ShellModel {
        async fn stream(
            &self,
            _spec: &llm::model::ModelSpec,
            request: &llm::request::Request,
        ) -> llm::Result<futures::stream::BoxStream<'static, llm::Result<StreamEvent>>> {
            let step = self.step.fetch_add(1, Ordering::SeqCst);
            let result = || {
                let result = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::User { content } => {
                            content.iter().find_map(|content| match content {
                                UserContent::ToolResult(result) => Some(result),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .expect("a tool result reached the model");
                assert!(!result.is_error, "{}", result.flatten_text());
                result.flatten_text()
            };
            let (name, args): (&str, Value) = match step {
                0 => (
                    toolbox::bash::Bash::NAME,
                    json!({
                        "command": "echo start >> starts; echo ready; while [ ! -f release ]; do sleep 0.01; done; echo finished; exit 3"
                    }),
                ),
                1 => {
                    assert!(result().contains("job #1"));
                    (
                        toolbox::bash::Bash::NAME,
                        json!({"join": 1, "timeout_ms": 20}),
                    )
                }
                2 => {
                    assert!(result().contains("still running"));
                    std::fs::write(self.root.join("release"), "").unwrap();
                    (
                        toolbox::bash::Bash::NAME,
                        json!({"join": 1, "timeout_ms": 5000}),
                    )
                }
                3 => {
                    let result = result();
                    assert!(
                        result.contains("exited 3") && result.contains("finished"),
                        "{result}"
                    );
                    return Ok(Box::pin(futures::stream::iter([
                        Ok(StreamEvent::BlockStart {
                            index: 0,
                            kind: BlockKind::Text,
                        }),
                        Ok(StreamEvent::TextDelta {
                            index: 0,
                            delta: "resolved".into(),
                        }),
                        Ok(StreamEvent::Done {
                            stop: StopReason::EndTurn,
                            usage: Default::default(),
                        }),
                    ])));
                }
                _ => panic!("the task already finished"),
            };
            Ok(Box::pin(futures::stream::iter([
                Ok(StreamEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::ToolCall {
                        id: Some(format!("call_{step}")),
                        name: name.into(),
                    },
                }),
                Ok(StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: args.to_string(),
                }),
                Ok(StreamEvent::Done {
                    stop: StopReason::ToolUse,
                    usage: Default::default(),
                }),
            ])))
        }
    }

    #[tokio::test]
    async fn the_model_can_continue_a_soft_timed_out_command_in_either_run() {
        for child in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
            let _stop = ctx.cancel.clone().drop_guard();
            let transport = Arc::new(ShellModel {
                step: AtomicUsize::new(0),
                root: dir.path().to_path_buf(),
            });
            let mut agent =
                agent::Agent::new(transport.clone(), crate::core::tests::test_spec("m"));
            super::super::lane::arm(
                &mut agent,
                super::super::lane::a_resolved(""),
                super::super::archive::nowhere(),
                agent::Retry::default(),
                None,
            );
            if child {
                let out = agent
                    .brief
                    .registry
                    .get(subagent::Subagent::NAME)
                    .unwrap()
                    .execute(
                        json!({"description": "run a command", "prompt": "run it"}),
                        &ctx,
                    )
                    .await
                    .unwrap();
                assert!(out.flatten().contains("resolved"));
            } else {
                let patch = Arc::make_mut(&mut agent.brief);
                patch.registry = scoped(&ctx, std::mem::take(&mut patch.registry));
                let mut session = agent::session::Session::with_prompt("run it");
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                agent
                    .run(&mut session, &ctx, &tx, &agent::Retry::default())
                    .await
                    .unwrap();
            }
            assert_eq!(transport.step.load(Ordering::SeqCst), 4);
            assert!(
                !ctx.cancel.is_cancelled(),
                "the child's cleanup leaves its parent running"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("starts")).unwrap(),
                "start\n"
            );
        }
    }
}
