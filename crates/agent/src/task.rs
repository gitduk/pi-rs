use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::unbounded_channel;
use tools::{Ctx, Tier, Tool, ToolError, ToolOutput};

use crate::event::{Event, Totals};
use crate::session::Session;
use crate::{Agent, AgentError};

pub const PROMPT: &str = include_str!("../prompts/task.md");

/// Where a finished subagent's work goes.
///
/// This layer says what it needs and the surface provides it: the store and the
/// reckoning both live out there, and reaching them from here would mean
/// knowing what a workspace directory or a status line is.
pub trait Home: Send + Sync {
    /// A subagent has no screen, so its transcript is the only account of what
    /// it did. Called once, whether the run finished or was cut short.
    fn keep(&self, id: &str, session: &Session);
    /// What it spent. Held by the implementor, because the same side reads it.
    fn spent(&self, totals: &Totals);
}

/// Ids only have to be distinct inside one process; the parent's own namespace
/// makes them distinct across runs.
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Deserialize)]
struct Args {
    prompt: String,
}

/// A whole agent loop behind one tool call.
///
/// The caller sees a tool that takes prose and answers with prose. What happens
/// in between is a second agent with a window of its own — which is the point:
/// a long search costs the caller one paragraph instead of forty turns.
pub struct Task {
    /// Cloned for each call and thrown away after. Its registry has no `task`
    /// of its own, so this does not nest.
    agent: Arc<Agent>,
    home: Arc<dyn Home>,
    /// Two limits, because they stop different things: turns stop a loop that
    /// keeps failing, the deadline stops a single call that has wedged.
    max_turns: usize,
    deadline: Duration,
}

impl Task {
    pub const NAME: &'static str = "task";

    /// Build the subagent from the one that will call it: same transport, same
    /// model, same ceiling, its own prompt, and no `task` in its registry.
    ///
    /// `standing` is what the checkout says — the workspace anchor and the
    /// instruction files. It travels with the tree, not with the caller, and
    /// the child edits that same tree.
    pub fn new(parent: &Agent, home: Arc<dyn Home>, standing: &str) -> Self {
        let mut agent = parent.clone();
        agent.registry = std::mem::take(&mut agent.registry).without(Self::NAME);
        agent.system = format!("{PROMPT}{standing}");
        Self {
            agent: Arc::new(agent),
            home,
            max_turns: 20,
            deadline: Duration::from_secs(600),
        }
    }

    pub fn with_limits(mut self, max_turns: usize, deadline: Duration) -> Self {
        self.max_turns = max_turns;
        self.deadline = deadline;
        self
    }
}

/// What the child's event stream said, once it has closed.
#[derive(Default)]
struct Heard {
    /// The last turn's prose only. Every earlier turn was followed by tool
    /// calls, which is what makes it not the answer.
    text: String,
    turns: usize,
    /// Accumulated per turn rather than taken from `run`, which hands back
    /// nothing when it ends early — and a run cut short has still been paid for.
    spent: Totals,
}

#[async_trait]
impl Tool for Task {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Hand a self-contained job to a subagent and get back its conclusion.\n\
         \n\
         The subagent works in this same checkout with the same tools, but in a \
         window of its own: everything it reads and runs stays there, and you \
         see only what it concludes. That is what it is for — a search that \
         would take twenty turns of yours costs you one paragraph.\n\
         \n\
         Send it work that is worth that trade: locating something across many \
         files, running a test suite and reporting what failed, reading a large \
         unfamiliar area and summarising it. Several at once is normal and they \
         run in parallel.\n\
         \n\
         Do not send it work you could do in a call or two — the round trip \
         costs more than the answer. Do not send it anything that needs you to \
         answer a question halfway through: it cannot reach you, and it will \
         guess. Say exactly what you want back, including the shape, because the \
         last thing it says is all you get.\n\
         \n\
         It cannot call this tool, so it cannot delegate further. Give it work \
         it can finish itself."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The whole job, self-contained: what to do, where to look, and what to report back. The subagent sees none of this conversation.",
                },
            },
            "required": ["prompt"],
            "additionalProperties": false,
        })
    }

    /// What the child may do, because that is what the caller is authorising.
    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tools::parse_args_hinted(args, "task takes `prompt`")?;

        let id = format!(
            "{}-task-{}",
            ctx.spill_namespace(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        // Per field, not wholesale: the tree, the locks and the renumbering are
        // shared because parent and child edit the same files, while the
        // transcript, its spills and the token are the child's own.
        let stop = ctx.cancel.child_token();
        let child = ctx.clone().with_cancel(stop.clone()).with_session(&id);

        let (tx, mut rx) = unbounded_channel();
        let cap = self.max_turns;
        let watch = stop.clone();
        let heard = tokio::spawn(async move {
            let mut heard = Heard::default();
            while let Some(event) = rx.recv().await {
                match event {
                    Event::TurnStart { turn } => {
                        heard.turns = turn;
                        heard.text.clear();
                        if turn > cap {
                            watch.cancel();
                        }
                    }
                    Event::TextDelta(text) => heard.text.push_str(&text),
                    Event::TurnEnd { usage, cost } => heard.spent.add(&usage, cost),
                    _ => {}
                }
            }
            heard
        });
        let mut session = Session::with_prompt(args.prompt);
        // Trips the same token as the turn cap rather than dropping the
        // future, so both endings unwind the run the way Esc does.
        let ran = {
            let mut run = std::pin::pin!(self.agent.run(&mut session, &child, &tx));
            match tokio::time::timeout(self.deadline, &mut run).await {
                Ok(ran) => ran,
                Err(_) => {
                    stop.cancel();
                    run.await
                }
            }
        };
        // The collector ends when the last sender goes, and `run` held one.
        drop(tx);
        let heard = heard.await.unwrap_or_default();

        self.home.spent(&heard.spent);
        self.home.keep(&id, &session);

        let cut = match ran {
            Ok(_) => None,
            // Esc, and only Esc: our own token being tripped leaves the
            // parent's alone. This is the one error the loop never hands back
            // to the model, so telling them apart is what stops a cap from
            // ending the caller's whole turn.
            Err(AgentError::Cancelled) if ctx.cancel.is_cancelled() => {
                return Err(ToolError::Cancelled);
            }
            Err(AgentError::Cancelled) => Some(if heard.turns > self.max_turns {
                format!("stopped at turn {} of {}", heard.turns, self.max_turns)
            } else {
                format!("stopped after {}s", self.deadline.as_secs())
            }),
            Err(why) => return Err(ToolError::Invalid(format!("task: {why}"))),
        };

        Ok(ToolOutput::text(answer(&heard, cut.as_deref())).with_preview(format!(
            "{} turn{} · {}",
            heard.turns,
            if heard.turns == 1 { "" } else { "s" },
            brain::count::in_out(heard.spent.usage.input, heard.spent.usage.output)
        )))
    }
}

/// What the caller reads. Never empty: a subagent that said nothing is a fact
/// the caller has to be told, not an empty string it has to interpret.
fn answer(heard: &Heard, cut: Option<&str>) -> String {
    let said = heard.text.trim();
    let body = if said.is_empty() {
        format!(
            "The subagent ran {} turn(s) and ended without saying anything.",
            heard.turns
        )
    } else {
        said.to_string()
    };
    match cut {
        Some(why) => format!("{body}\n\n[unfinished — {why}]"),
        None => body,
    }
}
