use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{mpsc::unbounded_channel, watch};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};
use toolbox::bash;

use agent::session::Session;
use agent::{Agent, AgentError, Archive, Briefing, Event, Retry};
use tracing::Instrument as _;

const PROMPT: &str = include_str!("../prompts/subagent.md");

// Ids only have to be distinct inside one process; the parent's own namespace
// makes them distinct across runs.
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Deserialize)]
struct Args {
    // Never read by the child — this is the caller's word to the screen and
    // the journal, which otherwise show a delegated job as a bare `subagent`.
    description: String,
    prompt: String,
    // Run after the child stops, in the same checkout. Never seen by the
    // child: a check it knows about is a check it can write itself around.
    #[serde(default)]
    verify: Option<String>,
    #[serde(default)]
    background: bool,
}

// How many written paths the result names before counting the rest — kept
// small since it rides on every result, unlike `spill::fit`.
const NAMED: usize = 20;

// What ran after the child, and how it went.
struct Checked {
    command: String,
    outcome: Outcome,
}

// A check that ran and one that never got to run are different answers, and
// so is the text each carries: what the command printed, or why nothing did.
enum Outcome {
    Ran { code: i32, body: String },
    // Killed at the cap. It ran, possibly far enough to leave changes behind,
    // and only its verdict is missing — never say this one did not run.
    CutOff { ms: u64 },
    // No verdict, for a reason that is not the cap. Silent on whether the
    // command ran: guessing risks telling a caller the tree is clean when it isn't.
    NoVerdict(String),
}

/// A whole agent loop behind one tool call.
///
/// The caller sees a tool that takes prose and answers with prose. What happens
/// in between is a second agent with a window of its own — which is the point:
/// a long search costs the caller one paragraph instead of forty turns.
#[derive(Clone)]
pub struct Subagent {
    // Cloned for each call and thrown away after. Its registry has no
    // `subagent` of its own, so this does not nest.
    agent: Arc<Agent>,
    archive: Arc<dyn Archive>,
    // How long the child may run silent before it is read as wedged: the one
    // brake here, and the only way a child ends that is not esc.
    deadline: Duration,
    // The retry schedule the child runs on, handed in when the tool is hung:
    // a tool has no way to reach the config where it is called.
    retry: Retry,
    // Whether `background` is offered; without it every call blocks its turn.
    background: bool,
}

impl Subagent {
    pub const NAME: &'static str = "subagent";

    /// Build the subagent from the one that will call it: same transport, same
    /// model, its own prompt, and no `subagent` in its registry.
    ///
    /// `standing` is the checkout's workspace anchor and instructions files.
    /// `brief` is passed in, not read off `parent`, so the caller can arm
    /// both from one value while the parent's own brief is still the old one.
    pub fn new(
        parent: &Agent,
        brief: Arc<Briefing>,
        archive: Arc<dyn Archive>,
        standing: &str,
        retry: Retry,
    ) -> Self {
        let deadline = brief
            .subagent_deadline
            .unwrap_or_else(|| Duration::from_secs(1800));
        let mut child = brief;
        let patch = Arc::make_mut(&mut child);
        patch.registry = std::mem::take(&mut patch.registry).without(Self::NAME);
        patch.system = format!("{PROMPT}{standing}");
        let agent = Agent {
            model: parent.model.clone(),
            brief: child,
            compactor: parent.compactor.clone(),
        };
        Self {
            agent: Arc::new(agent),
            archive,
            deadline,
            retry,
            background: false,
        }
    }

    /// Offer `background`: a call may leave its turn as a job of the
    /// context it runs under, its answer coming back as a turn of its own.
    pub fn with_background(mut self) -> Self {
        self.background = true;
        self
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

// What the child's event stream said, once it has closed.
#[derive(Default)]
struct Heard {
    // The last turn's prose only. Every earlier turn was followed by tool
    // calls, which is what makes it not the answer.
    text: String,
    turns: usize,
    // Accumulated per turn rather than taken from `run`, which hands back
    // nothing when it ends early — and a run cut short has still been paid for.
    spent: llm::stream::Usage,
    // The turn in flight's own count so far, until its end folds it into `spent`.
    turn: llm::stream::Usage,
    // The child's calls still out, oldest first.
    out: Vec<(String, String)>,
}

impl Heard {
    // `turn 3 · 41.2k/3.1k · grep`: where the child is, for the caller's row.
    fn progress(&self) -> String {
        let mut spent = self.spent;
        spent.add(&self.turn);
        let mut parts = vec![format!("turn {}", self.turns.max(1))];
        if spent.input + spent.output > 0 {
            parts.push(llm::figures::slash(spent.input, spent.output));
        }
        if let Some((_, name)) = self.out.last() {
            parts.push(name.clone());
        }
        parts.join(" · ")
    }
}

#[async_trait]
impl Tool for Subagent {
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
         Send one whenever the job has something checkable behind it: a test suite, a \
         build, a linter. What the subagent says about its own work is the only \
         part of the answer nothing else checks, and it cannot reach you to be \
         asked again."
    }

    fn schema(&self) -> Value {
        let mut schema = json!({
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
        });
        if self.background {
            schema["properties"]["background"] = json!({
                "type": "boolean",
                "description": "Run it apart from this turn: the call returns at once with an id, and the answer comes back later as a turn of its own. For a long job you need not wait on — keep working, or end the turn. It shares this checkout, so give it work that touches no file you are editing.",
            });
        }
        schema
    }

    // What the child may do, because that is what the caller is authorising.
    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args =
            tool::parse_args_hinted(args, "subagent takes `description` and `prompt`")?;
        // An ask with no text in it is a message every provider refuses; say so
        // here, where the caller can still change it.
        if args.prompt.trim().is_empty() {
            return Err(ToolError::Invalid(
                "the subagent's `prompt` is empty — say what it should do".into(),
            ));
        }
        if self.background && args.background {
            self.send_off(args, ctx)
        } else {
            self.run(args, ctx).await
        }
    }
}

impl Subagent {
    // Started apart from the turn: the table's token, so esc leaves it
    // running, and its own record of writes, since the caller's run will end.
    fn send_off(&self, args: Args, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let Some(sink) = ctx.jobs() else {
            return Err(ToolError::Invalid(
                "this run cannot keep a subagent in the background; call it without \
                 `background`"
                    .into(),
            ));
        };
        let description = args.description.replace('\n', " ");
        let stop = tokio_util::sync::CancellationToken::new();
        let job = sink.start(
            ctx.workspace.root().to_path_buf(),
            description.clone(),
            stop.clone(),
            None,
        );
        let id = job.id();
        let this = self.clone();
        let progress: tool::Progress = {
            let job = job.clone();
            Arc::new(move |said| job.status(said))
        };
        let ctx = ctx
            .clone()
            .with_own_writes()
            .with_cancel(stop)
            .with_progress(progress);
        tokio::spawn(async move {
            let (answer, spent) = match this.run(args, &ctx).await {
                Ok(out) => (format!("{}\n\n{}", out.preview(), out.flatten()), out.spent),
                Err(why) => (why.to_string(), Default::default()),
            };
            job.result(answer, spent);
            job.end();
        });
        let preview = format!("{} [background #{id}]", description.trim());
        Ok(ToolOutput::text(format!(
            "started in the background as #{id}; its answer comes back as a turn \
             of its own, so keep working or end this turn. `jobs` lists it, and \
             `jobs` with `stop` stops it."
        ))
        .with_preview(preview))
    }

    async fn run(&self, args: Args, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let id = format!(
            "{}-subagent-{}",
            ctx.spill_namespace(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        // Tree, locks and renumbering are shared with the parent (same files);
        // transcript, spill name and token are the child's own. It keeps no
        // jobs: nothing would bring their results back to the child.
        let stop = ctx.cancel.child_token();
        let root = ctx.spill_root().to_path_buf();
        let child = ctx
            .clone()
            .with_cancel(stop.clone())
            .with_session(&id, root)
            .with_own_writes()
            .without_jobs();

        let (tx, mut rx) = unbounded_channel();
        // Every event resets the silence clock, whatever kind it is: an
        // event is the child moving, and moving is all the watchdog asks.
        let (ticked, ticking) = watch::channel(Instant::now());
        let caller = ctx.clone();
        let heard = tokio::spawn(async move {
            let mut heard = Heard::default();
            let mut said = String::new();
            while let Some(event) = rx.recv().await {
                let _ = ticked.send(Instant::now());
                match event {
                    Event::TurnStart { turn } => {
                        heard.turns = turn;
                        heard.text.clear();
                        heard.turn = Default::default();
                    }
                    // The answer grows; where the child is does not.
                    Event::TextDelta(text) => {
                        heard.text.push_str(&text);
                        continue;
                    }
                    Event::Usage(usage) => heard.turn = usage,
                    Event::TurnEnd { usage, .. } => {
                        heard.spent.add(&usage);
                        heard.turn = Default::default();
                    }
                    Event::ToolStart { id, name, .. } => heard.out.push((id, name)),
                    Event::ToolEnd { id, .. } | Event::ToolDenied { id, .. } => {
                        heard.out.retain(|(out, _)| *out != id)
                    }
                    _ => {}
                }
                let now = heard.progress();
                if now != said {
                    caller.progress(now.clone());
                    said = now;
                }
            }
            heard
        });
        // The watchdog: silence for a whole deadline is a wedged call — a
        // hung tool speaks no events. It trips the same token esc does.
        let wedged = Arc::new(AtomicBool::new(false));
        let watchdog_stop = stop.clone();
        let wedged_flag = wedged.clone();
        let deadline = self.deadline;
        let watchdog = tokio::spawn(async move {
            let mut ticking = ticking;
            loop {
                let outlived = tokio::time::Instant::from(*ticking.borrow() + deadline);
                tokio::select! {
                    _ = tokio::time::sleep_until(outlived) => {
                        // A reset may have raced the trip; trust only the
                        // value read after the sleep came back.
                        if *ticking.borrow() + deadline <= Instant::now() {
                            wedged_flag.store(true, Ordering::Relaxed);
                            watchdog_stop.cancel();
                            return;
                        }
                    }
                    _ = ticking.changed() => {}
                    _ = watchdog_stop.cancelled() => return,
                }
            }
        });
        let mut session = Session::with_prompt(args.prompt);
        // One span for the whole child, so the journal can file its records
        // under it rather than lose them among its siblings'.
        let child_span = tracing::info_span!(
            target: "pi::subagent",
            "subagent",
            session = %child.spill_namespace(),
            description = %args.description,
        );
        // The token ends the run — the watchdog or esc — unwinding like an
        // esc; one ignoring it is dropped after STOP_GRACE.
        let started = Instant::now();
        let ran = {
            let mut run = std::pin::pin!(
                self.agent
                    .run(&mut session, &child, &tx, &self.retry)
                    .instrument(child_span)
            );
            let grace_stop = stop.clone();
            let outcome = tokio::select! {
                ran = &mut run => Some(ran),
                _ = async {
                    grace_stop.cancelled().await;
                    tokio::time::sleep(agent::STOP_GRACE).await;
                } => None,
            };
            match outcome {
                Some(ran) => ran,
                None => Err(AgentError::Unstopped),
            }
        };
        let took = started.elapsed();
        watchdog.abort();
        // The collector ends when the last sender goes, and `run` held one.
        drop(tx);
        let mut lost = false;
        let heard = match heard.await {
            Ok(heard) => heard,
            // Torn down with the task itself: nothing more was seen because
            // nothing more was sent.
            Err(why) if why.is_cancelled() => Heard::default(),
            Err(why) => {
                tracing::error!(target: "pi::subagent", error = %why, "the accounting collector panicked");
                lost = true;
                Heard::default()
            }
        };

        self.archive.keep(ctx.spill_namespace(), &id, session);

        let cut = match ran {
            Ok(_) => None,
            // Esc only: our own token tripping leaves the parent's alone —
            // conflating them would end the caller's whole turn over a wedged child.
            Err(AgentError::Cancelled) if ctx.cancel.is_cancelled() => {
                return Err(ToolError::Cancelled);
            }
            Err(AgentError::Cancelled) => Some(if wedged.load(Ordering::Relaxed) {
                format!(
                    "a call ran {} with no progress",
                    llm::figures::elapsed(self.deadline)
                )
            } else {
                format!("stopped after {}", llm::figures::elapsed(self.deadline))
            }),
            Err(why) => {
                // The child's spend rode home on the collector, but an error
                // result carries no `.with_spent`: name what went uncounted.
                let uncounted = if heard.spent.input + heard.spent.output > 0 {
                    format!(
                        " ({} turn(s), {}, ran uncounted)",
                        heard.turns,
                        llm::figures::slash(heard.spent.input, heard.spent.output)
                    )
                } else {
                    String::new()
                };
                return Err(ToolError::Invalid(format!("subagent: {why}{uncounted}")));
            }
        };

        let changed = child.writes();
        // Into the caller's record as well as the child's: these are changes
        // this run caused, and what reads that record asks exactly that.
        for path in &changed {
            ctx.note_write(path);
        }
        let wrote: Vec<String> = changed
            .iter()
            .map(|path| ctx.workspace.display(path))
            .collect();

        let check = match args
            .verify
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            None => None,
            Some(command) => {
                // No deadline of our own: `run` already clamps to the workspace's
                // shell cap, and a subagent's leash isn't a request for a shorter suite.
                let room = Duration::MAX;
                // The caller's context, not the child's: a child stopped by
                // its own cap leaves that token tripped, and this cancelled.
                let outcome = match bash::run(command, ctx.workspace.root(), room, ctx).await {
                    Ok(ran) => Outcome::Ran {
                        code: ran.code,
                        body: ran.body,
                    },
                    // Esc ends the caller's turn as elsewhere; anything else is an
                    // outcome, not a reason to drop the work the child already did.
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
        Ok(
            ToolOutput::text(answer(&heard, cut.as_deref(), lost, &wrote, check.as_ref()))
                .with_preview(sketch(&args.description, &heard, took))
                .with_spent(heard.spent),
        )
    }
}

// The finished-call line: job, then its cost in brackets. The job leads
// because several children run at once, so a bare cost names none of them.
fn sketch(description: &str, heard: &Heard, took: Duration) -> String {
    let spent = format!(
        "{} · {}",
        llm::figures::elapsed(took),
        llm::figures::slash(heard.spent.input, heard.spent.output)
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

// The child's words, then notes pulled from the tree rather than trusted
// from the child — write paths, check status. Never empty: silence is a fact too.
fn answer(
    heard: &Heard,
    cut: Option<&str>,
    lost: bool,
    wrote: &[String],
    check: Option<&Checked>,
) -> String {
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
    if lost {
        notes.push(
            "[token accounting lost — the collector crashed, this subagent's spend is not counted]"
                .to_string(),
        );
    }
    notes.push(wrote_line(wrote));
    if let Some(check) = check {
        notes.push(check_line(check));
    }
    format!("{body}\n\n{}", notes.join("\n"))
}

// The paths the child wrote — or that it wrote none, which is the line worth
// having when it has just finished describing the changes it made.
fn wrote_line(wrote: &[String]) -> String {
    let n = wrote.len();
    if n == 0 {
        return "[wrote nothing]".to_string();
    }
    let unit = if n == 1 { "file" } else { "files" };
    let named = wrote
        .iter()
        .take(NAMED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match n.saturating_sub(NAMED) {
        0 => format!("[wrote {n} {unit}: {named}]"),
        rest => format!("[wrote {n} {unit}: {named}, and {rest} more]"),
    }
}

// A passing check says all it has to with its exit status; a failing one is
// what the caller asked for, so its output comes too.
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
