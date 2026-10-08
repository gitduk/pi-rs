//! `[[hooks]]`: shell commands run around tool calls, each handed the call as
//! JSON on stdin. Before a call, exit 2 refuses it and what the hook printed
//! is the reason; after one, what it printed joins the result.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_store::config::{Hook, HookWhen};
use serde_json::{Value, json};
use tool::{Ctx, ToolOutput};

const WAIT: Duration = Duration::from_secs(30);

/// The hooks a config names, or none at all when it names none.
pub fn of(hooks: &[Hook]) -> Arc<dyn agent::Hooks> {
    if hooks.is_empty() {
        return Arc::new(agent::NoHooks);
    }
    Arc::new(Shell(hooks.to_vec()))
}

struct Shell(Vec<Hook>);

impl Shell {
    fn around<'a>(&'a self, when: HookWhen, tool: &'a str) -> impl Iterator<Item = &'a Hook> {
        self.0.iter().filter(move |h| {
            h.when == when && (h.tools.is_empty() || h.tools.iter().any(|t| t == tool))
        })
    }
}

#[async_trait]
impl agent::Hooks for Shell {
    async fn before(&self, tool: &str, args: &Value, ctx: &Ctx) -> Result<(), String> {
        let hooks: Vec<&Hook> = self.around(HookWhen::Before, tool).collect();
        if hooks.is_empty() {
            return Ok(());
        }
        let input = json!({ "tool": tool, "args": args, "workspace": ctx.workspace.root() });
        let input = input.to_string().into_bytes();
        for hook in hooks {
            match run(hook, &input, ctx).await {
                Ok((0, _)) => {}
                Ok((2, said)) => {
                    let said = if said.is_empty() {
                        "no reason given"
                    } else {
                        &said
                    };
                    return Err(format!("refused by a hook: {said}"));
                }
                // A guard that broke has not cleared the call: it stops
                // here, saying the hook is what to fix.
                Ok((code, said)) => return Err(broken(hook, &format!("exit {code}: {said}"))),
                Err(why) => return Err(broken(hook, &why)),
            }
        }
        Ok(())
    }

    async fn after(&self, tool: &str, args: &Value, out: &ToolOutput, ctx: &Ctx) -> Option<String> {
        let hooks: Vec<&Hook> = self.around(HookWhen::After, tool).collect();
        if hooks.is_empty() {
            return None;
        }
        let input = json!({
            "tool": tool,
            "args": args,
            "workspace": ctx.workspace.root(),
            "result": out.flatten(),
        });
        let input = input.to_string().into_bytes();
        let mut notes = Vec::new();
        for hook in hooks {
            match run(hook, &input, ctx).await {
                Ok((_, said)) if !said.is_empty() => notes.push(format!("[hook] {said}")),
                Ok(_) => {}
                Err(why) => warn(hook, &why),
            }
        }
        (!notes.is_empty()).then(|| notes.join("\n"))
    }
}

fn broken(hook: &Hook, why: &str) -> String {
    warn(hook, why);
    format!(
        "refused: the before-hook `{}` failed ({why}); the user has to fix it \
         in ~/.pi/settings.toml",
        hook.command
    )
}

fn warn(hook: &Hook, why: &str) {
    tracing::warn!(target: "pi::hooks", command = %hook.command, %why, "hook failed");
}

// The hook's exit code and what it printed, stdout then stderr. Run like a
// tool's child: its own group, bounded output, and esc stops it.
async fn run(hook: &Hook, input: &[u8], ctx: &Ctx) -> Result<(i32, String), String> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&hook.command)
        .current_dir(ctx.workspace.root());
    let exited = toolbox::process::run(cmd, Some(input.to_vec()), WAIT, ctx)
        .await
        .map_err(|e| e.to_string())?;
    let said = [&exited.stdout.text, &exited.stderr.text]
        .map(|t| t.trim())
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok((exited.status.code().unwrap_or(-1), said))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::Hooks;

    fn hook(when: HookWhen, command: &str) -> Hook {
        Hook {
            when,
            tools: vec!["bash".into()],
            command: command.into(),
        }
    }

    #[tokio::test]
    async fn exit_two_refuses_a_call_and_an_after_hook_adds_its_words() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let args = json!({ "command": "rm -rf /" });
        let hooks = Shell(vec![
            hook(
                HookWhen::Before,
                "grep -q 'rm -rf' && { echo no rm; exit 2; }; exit 0",
            ),
            hook(HookWhen::After, "echo checked"),
        ]);

        let refused = hooks.before("bash", &args, &ctx).await.unwrap_err();
        assert!(refused.contains("no rm"), "{refused}");
        assert!(
            hooks.before("read", &args, &ctx).await.is_ok(),
            "not a bash call"
        );
        let safe = json!({ "command": "ls" });
        assert!(hooks.before("bash", &safe, &ctx).await.is_ok());

        let note = hooks
            .after("bash", &safe, &ToolOutput::text("out"), &ctx)
            .await;
        assert_eq!(note.as_deref(), Some("[hook] checked"));

        let broken = Shell(vec![hook(HookWhen::Before, "exit 3")]);
        let refused = broken.before("bash", &safe, &ctx).await.unwrap_err();
        assert!(
            refused.contains("failed"),
            "a broken guard refuses: {refused}"
        );
    }
}
