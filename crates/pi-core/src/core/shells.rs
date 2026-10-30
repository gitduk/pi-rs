//! Shell executions owned by a single run rather than an interactive session.

use std::sync::Arc;

use tool::Registry;

/// Give a run a shell that waits for each command to exit. There are no
/// detached commands to control; cancellation releases the active process.
pub fn foreground(mut registry: Registry) -> Registry {
    registry.offer(Arc::new(toolbox::bash::Bash::foreground()));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tool::Ctx;

    use llm::message::{Message, UserContent};
    use llm::stream::{BlockKind, StopReason, StreamEvent};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn foreground_shell_respects_tool_cuts_and_keeps_its_name_over_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let _stop = ctx.cancel.clone().drop_guard();
        let full = foreground(toolbox::builtin());
        let shell = full.get(toolbox::bash::Bash::NAME).unwrap();
        assert!(shell.schema()["properties"].get("wait").is_none());
        assert!(shell.schema()["properties"].get("background").is_none());
        let read_only = foreground(toolbox::builtin().within(tool::Tier::Read));
        assert!(read_only.get(toolbox::bash::Bash::NAME).is_none());
        let hidden = foreground(toolbox::builtin().without(toolbox::bash::Bash::NAME));
        assert!(hidden.get(toolbox::bash::Bash::NAME).is_none());

        let script = dir.path().join("bash");
        std::fs::write(
            &script,
            "#!/bin/sh\n# ---\n# description = \"custom shell\"\n# ---\nprintf custom\n",
        )
        .unwrap();
        let mut registry = toolbox::builtin();
        registry.read(Arc::new(toolbox::scripts::Dir::new(dir.path())));
        let registry = foreground(registry);
        let shell = registry.get(toolbox::bash::Bash::NAME).unwrap();
        assert!(
            shell
                .execute(json!({"command": "echo builtin"}), &ctx)
                .await
                .unwrap()
                .flatten()
                .contains("builtin")
        );
    }

    #[test]
    fn foreground_shell_only_accepts_commands_and_describes_the_wait() {
        let registry = foreground(toolbox::builtin());
        let shell = registry.get(toolbox::bash::Bash::NAME).unwrap();
        let schema = shell.schema();
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 3);
        assert!(properties.contains_key("command"));
        assert!(properties.contains_key("cwd"));
        assert_eq!(schema["required"], json!(["command"]));
        assert!(shell.description().contains("Wait for the command to exit"));
        assert!(!shell.description().contains("command id"));
    }

    struct ShellModel {
        step: AtomicUsize,
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
                        "command": "echo start >> starts; echo ready; sleep 0.02; echo finished; exit 3"
                    }),
                ),
                1 => {
                    let result = result();
                    assert!(
                        result.contains("exit 3") && result.contains("finished"),
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
    async fn foreground_commands_finish_in_the_call_in_both_run_types() {
        for child in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
            let _stop = ctx.cancel.clone().drop_guard();
            let transport = Arc::new(ShellModel {
                step: AtomicUsize::new(0),
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
                patch.registry = foreground(std::mem::take(&mut patch.registry));
                let mut session = agent::session::Session::with_prompt("run it");
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                agent
                    .run(&mut session, &ctx, &tx, &agent::Retry::default())
                    .await
                    .unwrap();
            }
            assert_eq!(transport.step.load(Ordering::SeqCst), 2);
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
