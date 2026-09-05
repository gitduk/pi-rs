use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::unbounded_channel;
use tools::{Ctx, Tier, Tool, ToolError, ToolOutput, bash};

use crate::event::{Event, Totals};
use crate::session::Session;
use crate::{Agent, AgentError};
use tracing::Instrument as _;

pub const PROMPT: &str = include_str!("../prompts/task.md");

/// Where a finished subagent's work goes.
///
/// This layer says what it needs and the surface provides it; what a child
/// spent travels back on the tool result instead.
pub trait Home: Send + Sync {
    /// A subagent has no screen, so its transcript is the only account of what
    /// it did. Called once with the whole transcript, whether the run finished
    /// or was cut short.
    fn keep(&self, id: &str, session: Session);
}

/// Ids only have to be distinct inside one process; the parent's own namespace
/// makes them distinct across runs.
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Deserialize)]
struct Args {
    /// Never read by the child — this is the caller's word to the screen and
    /// the journal, which otherwise show a delegated job as a bare `task`.
    description: String,
    prompt: String,
    /// Run after the child stops, in the same checkout. Never seen by the
    /// child: a check it knows about is a check it can write itself around.
    #[serde(default)]
    verify: Option<String>,
}

/// How many of a child's written paths the result names before it counts the
/// rest. A note, not a view: `spill::fit` budgets a list the caller asked for,
/// where this one rides along on every result and has to stay small. The tree
/// is what a caller reads for the whole manifest.
const NAMED: usize = 20;

/// What ran after the child, and how it went.
struct Checked {
    command: String,
    outcome: Outcome,
}

/// A check that ran and one that never got to run are different answers, and
/// so is the text each carries: what the command printed, or why nothing did.
enum Outcome {
    Ran { code: i32, body: String },
    /// Killed at the cap. It ran, possibly far enough to leave changes behind,
    /// and only its verdict is missing — never say this one did not run.
    CutOff { ms: u64 },
    /// No verdict, for a reason that is not the cap. Deliberately silent on
    /// whether the command ran: a shell that would not start and an output
    /// that would not spill both arrive as one error, and guessing between
    /// them is how a caller is told the tree is clean when it is not.
    NoVerdict(String),
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
         it can finish itself.\n\
         \n\
         The result ends with every path it changed through `write` or `edit`, \
         and the exit status of `verify` if you gave one. A change it made by \
         running a command is not in that list, which is what `verify` covers. \
         Send one \
         whenever the job has something checkable behind it: a test suite, a \
         build, a linter. What the subagent says about its own work is the only \
         part of the answer nothing else checks, and it cannot reach you to be \
         asked again."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Three to six words naming the job, shown to the user while it runs — \"find every caller of spans\", \"run the test suite\". Not sent to the subagent.",
                },
                "prompt": {
                    "type": "string",
                    "description": "The whole job, self-contained: what to do, where to look, and what to report back. The subagent sees none of this conversation.",
                },
                "verify": {
                    "type": "string",
                    "description": "Shell command run in the workspace root after the subagent stops — a test suite, a build, a linter. Its exit status comes back with the result. Omit when nothing about the job is checkable.",
                },
            },
            "required": ["description", "prompt"],
            "additionalProperties": false,
        })
    }

    /// What the child may do, because that is what the caller is authorising.
    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tools::parse_args_hinted(args, "task takes `description` and `prompt`")?;

        let id = format!(
            "{}-task-{}",
            ctx.spill_namespace(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        // Per field, not wholesale: the tree, the locks and the renumbering are
        // shared because parent and child edit the same files, while the
        // transcript, its spills and the token are the child's own.
        let stop = ctx.cancel.child_token();
        let child = ctx
            .clone()
            .with_cancel(stop.clone())
            .with_session(&id)
            .with_own_writes();

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
        // One span for the whole child, so the journal can file its records
        // under it rather than lose them among its siblings'.
        let child_span = tracing::info_span!(
            target: "pi::task",
            "task",
            session = %child.spill_namespace(),
            description = %args.description,
        );
        // Trips the same token as the turn cap rather than dropping the
        // future, so both endings unwind the run the way Esc does.
        let ran = {
            let mut run = std::pin::pin!(
                self.agent
                    .run(&mut session, &child, &tx)
                    .instrument(child_span)
            );
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

        self.home.keep(&id, session);

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

        let wrote: Vec<String> = child
            .writes()
            .iter()
            .map(|path| ctx.workspace.display(path))
            .collect();

        let check = match args.verify.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            None => None,
            Some(command) => {
                // No deadline of our own: `run` clamps to the cap every shell
                // command in the workspace answers to, and shortening a
                // subagent's leash is not a request for a shorter test suite.
                let room = Duration::MAX;
                // The caller's context, not the child's: a child stopped by
                // its own cap leaves that token tripped, and this cancelled.
                let outcome = match bash::run(command, ctx.workspace.root(), room, ctx).await {
                    Ok(ran) => Outcome::Ran {
                        code: ran.code,
                        body: ran.body,
                    },
                    // Esc ends the caller's turn as it does anywhere else.
                    // Anything else is an outcome, not a reason to drop the
                    // work the child already did.
                    Err(ToolError::Cancelled) => return Err(ToolError::Cancelled),
                    Err(ToolError::Timeout { ms }) => Outcome::CutOff { ms },
                    Err(why) => Outcome::NoVerdict(why.to_string()),
                };
                Some(Checked {
                    // Flattened, since it is about to be quoted into one line.
                    command: command.replace('\n', " "),
                    outcome,
                })
            }
        };
        // The child's whole spend rides home on the result, where the parent's
        // run counts it — the surface never had a handle to drain.
        Ok(ToolOutput::text(answer(&heard, cut.as_deref(), &wrote, check.as_ref()))
            .with_preview(sketch(&args.description, &heard))
            .with_spent(heard.spent))
    }
}

/// The line a finished call leaves on the screen: the job it was, and in
/// brackets what it took. The job leads because several children run at once
/// and a bill alone names none of them; the brackets are what keep the bill
/// from reading as a second thing the caller asked for.
fn sketch(description: &str, heard: &Heard) -> String {
    let spent = format!(
        "{} turn{} · {}",
        heard.turns,
        if heard.turns == 1 { "" } else { "s" },
        brain::count::slash(heard.spent.usage.input, heard.spent.usage.output)
    );
    // Flattened, not trusted: this row is written one line at a time, and a
    // newline in it would stair-step everything drawn after.
    let job = description.replace('\n', " ");
    match job.trim() {
        // Nothing to hold apart from, so the brackets would enclose the row
        // rather than rank it.
        "" => spent,
        job => format!("{job} [{spent}]"),
    }
}

/// What the caller reads: the child's own words, then the notes that were not
/// taken from them.
///
/// A subagent's account of itself is the one part of this result nothing else
/// checks, and the caller cannot tell a job done from a job merely reported
/// done. `wrote` and `check` are taken from the tree instead — the paths from
/// the bookkeeping every write goes through, the status from running the
/// caller's own command afterwards. Never empty: a subagent that said nothing
/// is a fact the caller has to be told, not an empty string to interpret.
fn answer(heard: &Heard, cut: Option<&str>, wrote: &[String], check: Option<&Checked>) -> String {
    let said = heard.text.trim();
    let body = if said.is_empty() {
        format!(
            "The subagent ran {} turn(s) and ended without saying anything.",
            heard.turns
        )
    } else {
        said.to_string()
    };
    let mut notes = Vec::new();
    if let Some(why) = cut {
        notes.push(format!("[unfinished — {why}]"));
    }
    notes.push(wrote_line(wrote));
    if let Some(check) = check {
        notes.push(check_line(check));
    }
    format!("{body}\n\n{}", notes.join("\n"))
}

/// The paths the child wrote — or that it wrote none, which is the line worth
/// having when it has just finished describing the changes it made.
fn wrote_line(wrote: &[String]) -> String {
    let n = wrote.len();
    if n == 0 {
        return "[wrote nothing]".to_string();
    }
    let unit = if n == 1 { "file" } else { "files" };
    let named = wrote.iter().take(NAMED).map(String::as_str).collect::<Vec<_>>().join(", ");
    match n.saturating_sub(NAMED) {
        0 => format!("[wrote {n} {unit}: {named}]"),
        rest => format!("[wrote {n} {unit}: {named}, and {rest} more]"),
    }
}

/// A passing check says all it has to with its exit status; a failing one is
/// what the caller asked for, so its output comes too.
fn check_line(check: &Checked) -> String {
    let command = &check.command;
    match &check.outcome {
        Outcome::Ran { code: 0, .. } => format!("[verify `{command}`: exit 0]"),
        Outcome::Ran { code, body } => {
            let printed = body.trim_end();
            let mut line = format!("[verify `{command}`: exit {code}]");
            if !printed.is_empty() {
                line.push('\n');
                line.push_str(printed);
            }
            line
        }
        Outcome::CutOff { ms } => {
            format!("[verify `{command}`: no verdict — killed after {ms}ms, having run that long]")
        }
        Outcome::NoVerdict(why) => format!("[verify `{command}`: no verdict — {why}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Checked, NAMED, Outcome, check_line, wrote_line};

    fn paths(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("src/f{i}.rs")).collect()
    }

    #[test]
    fn a_ledger_names_what_it_can_and_counts_the_rest() {
        assert_eq!(wrote_line(&[]), "[wrote nothing]");
        assert_eq!(wrote_line(&paths(1)), "[wrote 1 file: src/f0.rs]");

        let many = wrote_line(&paths(NAMED + 3));
        assert!(many.starts_with(&format!("[wrote {} files: src/f0.rs,", NAMED + 3)), "{many}");
        assert!(many.ends_with(", and 3 more]"), "{many}");
        // The count is the whole of it, so a caller reading a truncated list
        // still knows how much it is not being shown.
        assert_eq!(many.matches("src/f").count(), NAMED, "{many}");
    }

    /// The cap is ten minutes, so the endings a caller most needs told apart
    /// are the two no run in a test can reach.
    #[test]
    fn a_check_that_may_have_run_is_never_reported_as_one_that_did_not() {
        let of = |outcome| check_line(&Checked { command: "cargo test".into(), outcome });

        let killed = of(Outcome::CutOff { ms: 600_000 });
        assert!(killed.contains("no verdict"), "{killed}");
        // It ran, and may have left changes behind. Saying it did not is how a
        // caller concludes the tree is untouched when it is not.
        assert!(!killed.contains("did not run"), "{killed}");
        assert!(killed.contains("600000ms"), "{killed}");

        let lost = of(Outcome::NoVerdict("no such shell".into()));
        assert!(lost.contains("no verdict — no such shell"), "{lost}");
        // The same reason: it is not known whether this one ran either.
        assert!(!lost.contains("did not run"), "{lost}");

        let passed = of(Outcome::Ran { code: 0, body: "quiet".into() });
        assert_eq!(passed, "[verify `cargo test`: exit 0]", "no output when it passed");

        let failed = of(Outcome::Ran { code: 101, body: "assertion failed\n".into() });
        assert_eq!(failed, "[verify `cargo test`: exit 101]\nassertion failed");
    }
}
