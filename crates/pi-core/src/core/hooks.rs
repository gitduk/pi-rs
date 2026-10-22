//! `[[hooks]]`: shell commands run on what a run does, each handed the event
//! as JSON on stdin. Around a tool call they are gates: before one, exit 2
//! refuses it and what the hook printed is the reason; after one, what it
//! printed joins the result. As a run starts they are waited on, and what
//! they print is a note to the model; as it would stop, exit 2 keeps it
//! going. Every other event they only watch, queued off the loop: each hook
//! runs its events one at a time, in order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent::Event;
use async_trait::async_trait;
use pi_store::config::{Hook, HookOn};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tool::{Ctx, ToolOutput};

const WAIT: Duration = Duration::from_secs(30);

// Events waiting on watching hooks. Past this a hook is not keeping up, and
// what it misses is journaled rather than held without bound.
const QUEUE: usize = 256;

// How long leaving waits on watching hooks still queued or running.
const LEAVING: Duration = Duration::from_secs(5);

// Events queued and not yet run, across every config's hooks, so leaving
// can wait them out.
static PENDING: AtomicUsize = AtomicUsize::new(0);
static DRAINED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Wait, a few seconds at most, for watching hooks to run what was queued:
/// the last events of a run are said just before pi leaves.
pub async fn flush() {
    let _ = tokio::time::timeout(LEAVING, async {
        loop {
            let drained = DRAINED.notified();
            if PENDING.load(Ordering::Acquire) == 0 {
                return;
            }
            drained.await;
        }
    })
    .await;
}

/// The hooks a config names, or none at all when it names none. Watching
/// hooks run in `workspace`, as the gates run in their call's.
pub fn of(hooks: &[Hook], workspace: &tool::Workspace) -> Arc<dyn agent::Hooks> {
    if hooks.is_empty() {
        return Arc::new(agent::NoHooks);
    }
    Arc::new(Shell {
        hooks: hooks.to_vec(),
        ctx: Ctx::new(workspace.clone()),
        queues: OnceLock::new(),
    })
}

struct Shell {
    hooks: Vec<Hook>,
    // What watching hooks run under: the workspace, and no stop of a run's.
    ctx: Ctx,
    // One per hook that watches anything, so a slow one holds up only its
    // own events. Started by the first event watched, on the run's runtime.
    queues: OnceLock<Vec<Option<mpsc::Sender<Vec<u8>>>>>,
}

impl Shell {
    fn around<'a>(&'a self, on: HookOn, tool: &'a str) -> impl Iterator<Item = &'a Hook> {
        self.on(on, Some(tool)).map(|(_, h)| h)
    }

    // The hooks for `on`, by index; a `tool.*` event's only when about one
    // of the hook's tools.
    fn on<'a>(
        &'a self,
        on: HookOn,
        tool: Option<&'a str>,
    ) -> impl Iterator<Item = (usize, &'a Hook)> {
        self.hooks.iter().enumerate().filter(move |(_, h)| {
            h.on.contains(&on)
                && (h.tools.is_empty() || tool.is_none_or(|t| h.tools.iter().any(|n| n == t)))
        })
    }

    // The hooks a run waits on at `on`, which no tool narrows.
    fn waited(&self, on: HookOn) -> Vec<&Hook> {
        self.on(on, None).map(|(_, h)| h).collect()
    }

    fn queues(&self) -> Option<&[Option<mpsc::Sender<Vec<u8>>>]> {
        if let Some(queues) = self.queues.get() {
            return Some(queues);
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let gate = |on: &HookOn| {
            matches!(
                on,
                HookOn::ToolBefore | HookOn::ToolAfter | HookOn::RunStart | HookOn::RunStop
            )
        };
        let started = (self.hooks.iter())
            .map(|hook| {
                if hook.on.iter().all(gate) {
                    return None;
                }
                let (tx, rx) = mpsc::channel(QUEUE);
                runtime.spawn(watch(rx, hook.clone(), self.ctx.clone()));
                Some(tx)
            })
            .collect();
        // A racing first event may have started a set too: its senders are
        // dropped here, and its workers end with nothing to do.
        Some(self.queues.get_or_init(|| started))
    }
}

// Runs what was queued for `hook`, one at a time, until the hooks are replaced.
async fn watch(mut rx: mpsc::Receiver<Vec<u8>>, hook: Hook, ctx: Ctx) {
    while let Some(input) = rx.recv().await {
        let hook = &hook;
        match ran(hook, &input, &ctx).await {
            Some((0, _)) | None => {}
            Some((code, said)) => warn(hook, &format!("exit {code}: {said}")),
        }
        if PENDING.fetch_sub(1, Ordering::AcqRel) == 1 {
            DRAINED.notify_waiters();
        }
    }
}

// An event as a hook watches it: its name, its tool, its own fields. Calls
// are their gates'; the rest come too often to be events.
fn watched(event: &Event) -> Option<(HookOn, Option<&str>, Value)> {
    let usage = |u: &llm::stream::Usage| {
        json!({
            "input": u.input,
            "output": u.output,
            "cache_read": u.cache_read,
            "cache_write": u.cache_write,
        })
    };
    let (on, own) = match event {
        Event::ToolDenied { id, name, reason } => {
            let own = json!({ "id": id, "tool": name, "reason": reason });
            return Some((HookOn::ToolDenied, Some(name), own));
        }
        Event::TurnStart { turn } => (HookOn::TurnStart, json!({ "turn": turn })),
        Event::TurnEnd {
            usage: u,
            text,
            generating,
        } => (
            HookOn::TurnEnd,
            json!({
                "text": text,
                "usage": usage(u),
                "generating_ms": generating.map(|d| d.as_millis() as u64),
            }),
        ),
        Event::Compacted(r) => (HookOn::Compacted, json!({ "summarized": r.summarized })),
        Event::Retrying {
            attempt,
            delay_ms,
            reason,
        } => (
            HookOn::Retrying,
            json!({ "attempt": attempt, "delay_ms": delay_ms, "reason": reason }),
        ),
        Event::Warning(text) | Event::Refit(text) => (HookOn::Warning, json!({ "text": text })),
        Event::Failed { error } => (HookOn::RunFailed, json!({ "error": error })),
        Event::Done {
            turns,
            usage: u,
            ctx,
            compactions,
        } => (
            HookOn::RunDone,
            json!({
                "turns": turns,
                "usage": usage(u),
                "context": ctx.map(|(used, window)| json!({ "used": used, "window": window })),
                "compactions": compactions,
            }),
        ),
        // A tool's results land at its gates; a state update is no event.
        Event::ToolStart { .. } | Event::ToolEnd { .. } | Event::Committed { .. } => return None,
        Event::TextDelta(_)
        | Event::ReasoningDelta(_)
        | Event::ToolProgress { .. }
        | Event::Usage(_)
        | Event::Context { .. } => return None,
    };
    Some((on, None, own))
}

#[async_trait]
impl agent::Hooks for Shell {
    async fn before(&self, tool: &str, args: &Value, ctx: &Ctx) -> Result<(), String> {
        let hooks: Vec<&Hook> = self.around(HookOn::ToolBefore, tool).collect();
        if hooks.is_empty() {
            return Ok(());
        }
        let input = line(&json!({ "tool": tool, "args": args, "workspace": ctx.workspace.root() }));
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
        let hooks: Vec<&Hook> = self.around(HookOn::ToolAfter, tool).collect();
        if hooks.is_empty() {
            return None;
        }
        let input = line(&json!({
            "tool": tool,
            "args": args,
            "workspace": ctx.workspace.root(),
            "result": out.flatten(),
        }));
        notes(hooks, &input, ctx, |_| true).await
    }

    async fn ahead(&self, prompt: &str, ctx: &Ctx) -> Option<String> {
        let hooks = self.waited(HookOn::RunStart);
        if hooks.is_empty() {
            return None;
        }
        let input = line(&json!({
            "event": HookOn::RunStart,
            "workspace": ctx.workspace.root(),
            "prompt": prompt,
        }));
        // A failure's words are for whoever fixes the hook, not the model.
        notes(hooks, &input, ctx, |code| code == 0).await
    }

    async fn hold(&self, reply: &str, held: usize, ctx: &Ctx) -> Option<String> {
        let hooks = self.waited(HookOn::RunStop);
        if hooks.is_empty() {
            return None;
        }
        let input = line(&json!({
            "event": HookOn::RunStop,
            "workspace": ctx.workspace.root(),
            "text": reply,
            "held": held,
        }));
        for hook in hooks {
            match ran(hook, &input, ctx).await {
                Some((0, _)) | None => {}
                Some((2, said)) if said.is_empty() => return Some("[hook] not done yet".into()),
                Some((2, said)) => return Some(format!("[hook] {said}")),
                // Unlike a call's guard, a broken one lets the run stop: held
                // by it, the run could only go round until the cap.
                Some((code, said)) => warn(hook, &format!("exit {code}: {said}")),
            }
        }
        None
    }

    fn saw(&self, event: &Event) {
        let Some((on, tool, own)) = watched(event) else {
            return;
        };
        let mut hooks = self.on(on, tool).map(|(at, _)| at).peekable();
        if hooks.peek().is_none() {
            return;
        }
        let Some(queues) = self.queues() else {
            return;
        };
        let mut input = json!({ "event": on, "workspace": self.ctx.workspace.root() });
        if let (Value::Object(head), Value::Object(own)) = (&mut input, own) {
            head.extend(own);
        }
        let input = line(&input);
        for at in hooks {
            let Some(queue) = &queues[at] else {
                continue;
            };
            PENDING.fetch_add(1, Ordering::AcqRel);
            if queue.try_send(input.clone()).is_err() {
                PENDING.fetch_sub(1, Ordering::AcqRel);
                warn(&self.hooks[at], "not keeping up; an event was dropped");
            }
        }
    }
}

// An event as a hook reads it: one line of JSON.
fn line(input: &Value) -> Vec<u8> {
    format!("{input}\n").into_bytes()
}

// What each hook printed, as notes for the model; output from an exit
// `kept` refuses is journaled instead.
async fn notes(
    hooks: Vec<&Hook>,
    input: &[u8],
    ctx: &Ctx,
    kept: fn(i32) -> bool,
) -> Option<String> {
    let mut notes = Vec::new();
    for hook in hooks {
        match ran(hook, input, ctx).await {
            Some((code, said)) if kept(code) => {
                if !said.is_empty() {
                    notes.push(format!("[hook] {said}"));
                }
            }
            Some((code, said)) => warn(hook, &format!("exit {code}: {said}")),
            None => {}
        }
    }
    (!notes.is_empty()).then(|| notes.join("\n"))
}

// `run`, with a hook that could not be run journaled and passed over.
async fn ran(hook: &Hook, input: &[u8], ctx: &Ctx) -> Option<(i32, String)> {
    run(hook, input, ctx)
        .await
        .map_err(|why| warn(hook, &why))
        .ok()
}

fn broken(hook: &Hook, why: &str) -> String {
    warn(hook, why);
    format!(
        "refused: the before-hook `{}` failed ({why}); the user has to fix it \
         where it is configured",
        hook.command
    )
}

fn warn(hook: &Hook, why: &str) {
    tracing::warn!(target: "pi::hooks", command = %hook.command, %why, "hook failed");
}

// The hook's exit code and what it printed, stdout then stderr. Run like a
// tool's child: its own group, bounded output, and a stop ends it.
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

    fn shell(ctx: &Ctx, hooks: Vec<Hook>) -> Arc<dyn Hooks> {
        of(&hooks, &ctx.workspace)
    }

    fn hook(on: HookOn, command: &str) -> Hook {
        Hook {
            on: vec![on],
            tools: vec!["bash".into()],
            command: command.into(),
        }
    }

    #[tokio::test]
    async fn exit_two_refuses_a_call_and_an_after_hook_adds_its_words() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let args = json!({ "command": "rm -rf /" });
        let hooks = shell(
            &ctx,
            vec![
                hook(
                    HookOn::ToolBefore,
                    "grep -q 'rm -rf' && { echo no rm; exit 2; }; exit 0",
                ),
                hook(HookOn::ToolAfter, "echo checked"),
            ],
        );

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

        let broken = shell(&ctx, vec![hook(HookOn::ToolBefore, "exit 3")]);
        let refused = broken.before("bash", &safe, &ctx).await.unwrap_err();
        assert!(
            refused.contains("failed"),
            "a broken guard refuses: {refused}"
        );
    }

    // Watching is off the loop: nothing waits on it, so only a test sees
    // whether the event got there, and as one line of the agreed shape.
    #[tokio::test]
    async fn a_watching_hook_hears_its_event_and_not_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let mut end = hook(HookOn::TurnEnd, "cat >> heard");
        end.tools.clear();
        let hooks = shell(&ctx, vec![end]);

        hooks.saw(&Event::TextDelta("hi".into()));
        hooks.saw(&Event::TurnStart { turn: 1 });
        hooks.saw(&Event::TurnEnd {
            usage: llm::stream::Usage {
                output: 7,
                ..Default::default()
            },
            text: "done".into(),
            generating: None,
        });

        let heard = dir.path().join("heard");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let written = || std::fs::read_to_string(&heard).is_ok_and(|s| s.ends_with('\n'));
        while !written() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let said = std::fs::read_to_string(&heard).unwrap();
        let lines: Vec<Value> = said
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "{said}");
        assert_eq!(lines[0]["event"], "turn.end");
        assert_eq!(lines[0]["usage"]["output"], 7);
        assert_eq!(lines[0]["text"], "done");
    }

    #[tokio::test]
    async fn a_run_start_hook_tells_the_model_only_what_a_success_printed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let mut said = hook(
            HookOn::RunStart,
            "grep -q '\"prompt\":\"go\"' && echo ran ls",
        );
        let mut quiet = hook(HookOn::RunStart, "true");
        let mut failed = hook(HookOn::RunStart, "echo broken; exit 1");
        for h in [&mut said, &mut quiet, &mut failed] {
            h.tools.clear();
        }
        let hooks = shell(&ctx, vec![said, quiet, failed]);
        assert_eq!(
            hooks.ahead("go", &ctx).await.as_deref(),
            Some("[hook] ran ls")
        );

        let none = shell(&ctx, vec![hook(HookOn::TurnEnd, "echo no")]);
        assert_eq!(none.ahead("go", &ctx).await, None);
    }

    #[tokio::test]
    async fn a_stop_hook_holds_only_on_exit_two() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let gate = |command: &str| {
            let mut h = hook(HookOn::RunStop, command);
            h.tools.clear();
            shell(&ctx, vec![h])
        };
        let tests = gate(
            "in=$(cat); echo \"$in\" | grep -q '\"held\":1' && echo \"$in\" | grep -q 'all done' || exit 0; echo run the tests; exit 2",
        );
        assert_eq!(
            tests.hold("all done", 1, &ctx).await.as_deref(),
            Some("[hook] run the tests")
        );
        assert_eq!(tests.hold("all done", 0, &ctx).await, None);
        assert_eq!(
            gate("exit 2").hold("x", 0, &ctx).await.as_deref(),
            Some("[hook] not done yet")
        );
        assert_eq!(gate("echo broken; exit 1").hold("x", 0, &ctx).await, None);
    }

    #[test]
    fn a_failed_run_is_watched_with_its_error() {
        let (on, _, own) = watched(&Event::Failed {
            error: "gone".into(),
        })
        .unwrap();
        assert_eq!(on, HookOn::RunFailed);
        assert_eq!(own["error"], "gone");
    }
}
