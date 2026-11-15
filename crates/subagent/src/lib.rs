use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ::execution::background::{Invocation, Outcome as ExecutionOutcome, Report, RunSink};
use async_trait::async_trait;
use pi_tools::bash;
use serde_json::{Value, json};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

use agent::session::Session;
use agent::{Agent, AgentError, Archive, Briefing, Event, Retry};
use tracing::Instrument as _;

/// The role-specific part of the built-in subagent prompt.
pub const PROMPT: &str = include_str!("../prompts/subagent.md");

/// The default role and shared execution policy, ahead of the checkout's instructions.
pub fn default_prompt() -> String {
    format!("{PROMPT}\n\n{}", agent::EXECUTION)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    // Never read by the child — this is the caller's word to the user and
    // the log, which otherwise show a delegated task as a bare `subagent`.
    description: String,
    prompt: String,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<NonZeroU64>,
    // Run after the child stops, in the same checkout. Never seen by the
    // child: a check it knows about is a check it can write itself around.
    #[serde(default)]
    verify: Option<String>,
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
    // The retry schedule the child runs on, handed in when the tool is hung:
    // a tool has no way to reach the config where it is called.
    retry: Retry,
    tools: Option<ToolFactory>,
    models: Option<ModelResolver>,
    skills: Option<Arc<skills::Shelf>>,
    executions: Option<Arc<dyn RunSink>>,
}

/// Build tools owned by one child run, released when its context is cancelled.
pub type ToolFactory = Arc<dyn Fn(&Ctx, tool::Registry) -> tool::Registry + Send + Sync>;

/// Resolve a model name using the caller's connection configuration.
pub type ModelResolver = Arc<dyn Fn(&str) -> Result<ModelConnection, ToolError> + Send + Sync>;

/// A child model's connection and diagnostics to preserve with its result.
pub struct ModelConnection {
    pub transport: Arc<dyn llm::Transport>,
    pub spec: llm::ModelSpec,
    pub notes: Vec<String>,
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
        Self::prompted(parent, brief, archive, &default_prompt(), standing, retry)
    }

    /// The same, under `prompt` in place of the built-in one.
    pub fn prompted(
        parent: &Agent,
        brief: Arc<Briefing>,
        archive: Arc<dyn Archive>,
        prompt: &str,
        standing: &str,
        retry: Retry,
    ) -> Self {
        let mut child = brief;
        let patch = Arc::make_mut(&mut child);
        patch.registry = std::mem::take(&mut patch.registry).without(Self::NAME);
        patch.system = format!("{prompt}{standing}");
        patch.hooks = Arc::new(agent::GatesOnly(patch.hooks.clone()));
        let agent = Agent {
            model: parent.model.clone(),
            brief: child,
            compactor: parent.compactor.clone(),
            meter: parent.meter.clone(),
        };
        Self {
            agent: Arc::new(agent),
            archive,
            retry,
            tools: None,
            models: None,
            skills: None,
            executions: None,
        }
    }

    /// Register calls that must continue independently of their calling turn.
    pub fn with_background(mut self, executions: Arc<dyn RunSink>) -> Self {
        self.executions = Some(executions);
        self
    }

    /// Arm each child with its own tools under that child's cancellation token.
    pub fn with_tools(mut self, tools: ToolFactory) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Allow each call to select a model through the caller's resolver.
    pub fn with_models(mut self, models: ModelResolver) -> Self {
        self.models = Some(models);
        self
    }

    /// Preload explicitly selected instructions from the caller's skill catalog.
    pub fn with_skills(mut self, skills: Arc<skills::Shelf>) -> Self {
        self.skills = Some(skills);
        self
    }
}

// What the child's event stream said, once it has closed.
#[derive(Default)]
struct Heard {
    warnings: Vec<String>,
    // The last turn's prose only. Every earlier turn was followed by tool
    // calls, which is what makes it not the answer.
    text: String,
    turns: usize,
    // Accumulated per turn rather than taken from `run`, which hands back
    // nothing when it ends early — and a run cut short has still been paid for.
    spent: llm::stream::Usage,
    // The turn in flight's own count so far, until its end folds it into `spent`.
    turn: llm::stream::Usage,
    doing: Doing,
}

// The child's latest call as its progress names it, held at least `HOLD`
// so progress updates are paced: a run of short calls does not replace the
// named call with each.
#[derive(Default)]
struct Doing {
    shown: String,
    at: Option<Instant>,
    next: Option<String>,
}

const HOLD: Duration = Duration::from_secs(1);

impl Doing {
    fn want(&mut self, call: String, now: Instant) {
        self.next = Some(call);
        self.settle(now);
    }

    // When a held call is owed its place in the progress, if one is waiting.
    fn due(&self) -> Option<Instant> {
        self.next.as_ref()?;
        Some(self.at? + HOLD)
    }

    fn settle(&mut self, now: Instant) {
        if self.at.is_some_and(|at| now.duration_since(at) < HOLD) {
            return;
        }
        if let Some(next) = self.next.take() {
            self.shown = next;
            self.at = Some(now);
        }
    }
}

impl Heard {
    // `turn 3 · 41.2k/3.1k · read src/lib.rs`: where the child is, for the
    // caller's progress. The call goes last, so a line cut short loses it first.
    fn progress(&self) -> String {
        let mut spent = self.spent;
        spent.add(&self.turn);
        let mut parts = vec![format!("turn {}", self.turns.max(1))];
        if spent.input + spent.output > 0 {
            parts.push(llm::figures::slash(spent.input, spent.output));
        }
        if !self.doing.shown.is_empty() {
            parts.push(self.doing.shown.clone());
        }
        parts.join(" · ")
    }
}

// The caller gets the child's progress only when it reads differently.
fn tell(caller: &Ctx, heard: &Heard, said: &mut String) {
    let progress = heard.progress();
    if progress != *said {
        caller.progress(progress.clone());
        *said = progress;
    }
}

// `read src/lib.rs`: the call's name and the one argument worth showing,
// cut to its first line since progress is a single line.
fn call_line(name: &str, args: &Value) -> String {
    match tool::summary(args).lines().next().map(str::trim) {
        Some(asked) if !asked.is_empty() => format!("{name} {asked}"),
        _ => name.to_string(),
    }
}

#[async_trait]
impl Tool for Subagent {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Assign a self-contained task to a subagent. \
         When background execution is available, tasks run in the background by default: the tool returns a task ID immediately and delivers the conclusion automatically. \
         Dispatch independent tasks, then continue other work or end your turn; do not wait or poll for them. \
         Set `background: false` only when the next action requires the conclusion immediately or the user explicitly requests synchronous execution. \
         Without background support, the tool waits and returns the conclusion. \
         The subagent uses the same checkout, with a separate context. \
         It inherits your model and tools unless you supply `model` or `tools`. \
         Supply `skills` to preload applicable instructions by name before the task starts. \
         Skills already loaded in your conversation are not inherited. \
         `tools` restricts callable tools to the listed names. \
         It cannot add tools you lack. \
         Select only the tools the task needs. \
         For read-only exploration, include only read tools and exclude shells and other tools that can change state. \
         You receive only its final message, not the files it reads or commands it runs.\n\n\
         Several calls can run in parallel.\n\n\
         Do not assign work that requires answers from you while it runs. \
         The subagent cannot ask you questions. \
         It will guess missing details. \
         Provide the complete task, necessary context, and required result format. \
         The subagent cannot call this tool or delegate further. \
         Assign work it can finish itself.\n\n\
         The result ends with paths changed through `write` or `edit`. \
         This list excludes changes made through shell commands.\n\n\
         Supply `verify` whenever the task has a check, such as tests, a build, or a linter. \
         The verification command's exit status accompanies the result. \
         This command can check changes made through shell commands. \
         The subagent's own report has no independent check. \
         You cannot ask the subagent further questions."
    }

    fn schema(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "background": {
                    "type": "boolean",
                    "default": true,
                    "description": "Defaults to true. Return a task ID immediately and deliver the conclusion to the originating session automatically. Use background execution for independent tasks even when you have no other work: dispatch them and end your turn. Set false only when the next action immediately requires the conclusion or the user explicitly requests synchronous execution. The task stops when this process exits."
                },
                "description": {
                    "type": "string",
                    "description": "Name the task in three to six words. Examples: \"find every caller of spans\" or \"run the test suite\". The user sees this label while the task runs. The subagent does not receive it.",
                },
                "prompt": {
                    "type": "string",
                    "description": "Describe the complete task. Specify what to do, where to search, and what to report. The subagent does not receive this conversation.",
                },
                "verify": {
                    "type": "string",
                    "description": "Supply a shell command to check the task, such as tests, a build, or a linter. The command runs in the workspace root after the subagent stops. Its exit status accompanies the result. Omit `verify` when the task has no check.",
                },
                "model": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Model to use for this task, optionally qualified by provider. Omit to inherit the caller's model. The connection must already be configured.",
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "uniqueItems": true,
                    "description": "Exact names of tools the child may call. Omit to inherit tools; an empty list disables all tools. Preloaded skills do not grant tools. This restricts tool access, not the side effects of an allowed tool. verify runs separately with the caller's permissions.",
                },
                "skills": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "uniqueItems": true,
                    "description": "Skill names whose full instructions and sources are loaded before the task starts. Missing or unreadable skills fail the call. Omit or pass [] to preload none. Preloading does not require the skill tool; allow that tool if the child needs supporting files or additional skills.",
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": tool::limit::MAX_CALL.as_millis(),
                    "default": tool::limit::DEFAULT_CALL.as_millis(),
                    "description": "Total child runtime budget in milliseconds, including periods with progress. Reaching the budget cancels the child and returns an unfinished result after cleanup. Verification runs separately.",
                },
            },
            "required": ["description", "prompt"],
            "additionalProperties": false,
        });
        if self.executions.is_none() {
            schema["properties"]
                .as_object_mut()
                .expect("object schema")
                .remove("background");
        }
        schema
    }

    // What the child may do, because that is what the caller is authorising.
    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, mut args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        if self.executions.is_some()
            && let Some(args) = args.as_object_mut()
        {
            args.entry("background").or_insert(Value::Bool(true));
        }
        let args: Args =
            tool::parse_args_hinted(args, "subagent takes `description` and `prompt`")?;
        // An ask with no text in it is a message every provider refuses; say so
        // here, where the caller can still change it.
        if args.prompt.trim().is_empty() {
            return Err(ToolError::Invalid(
                "the subagent's `prompt` is empty — say what it should do".into(),
            ));
        }
        if !args.background {
            let prepared = self.prepare(args, ctx).await?;
            return prepared
                .run(self.archive.as_ref(), &self.retry, ctx)
                .await
                .map(|report| report.output);
        }
        let executions = self.executions.as_ref().ok_or_else(|| {
            ToolError::Invalid("background execution is not available for this subagent".into())
        })?;
        // Validation remains cancellable; accepted work gets its own lifetime.
        let stop = tokio_util::sync::CancellationToken::new();
        let detached = ctx.clone().with_cancel(stop.clone());
        let mut prepared = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            prepared = self.prepare(args, &detached) => prepared?,
        };
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let label = format!(
            "{} · {}",
            prepared.agent.spec().model,
            prepared.args.description
        );
        let execution = executions.start(
            ctx.into(),
            Invocation::Task {
                name: Self::NAME.into(),
                description: label.clone(),
            },
            stop,
        );
        let id = execution.id();
        let progress = execution.clone();
        let progress: tool::Progress = Arc::new(move |text| progress.channel().status(text));
        let detached = detached.with_progress(progress.clone());
        prepared.child = prepared.child.with_progress(progress);
        let archive = self.archive.clone();
        let retry = self.retry;
        ::execution::background::spawn(execution, async move {
            let report = match prepared.run(archive.as_ref(), &retry, &detached).await {
                Ok(report) => report,
                Err(error) => Report {
                    outcome: ExecutionOutcome::Failed,
                    output: ToolOutput::text(error.to_string()).warned(),
                },
            };
            Report {
                output: tool::output::bound(report.output, &detached),
                ..report
            }
        });
        Ok(
            ToolOutput::text(json!({"task_id": id, "status": "running"}).to_string())
                .with_preview(format!("{label} [background #{id}]")),
        )
    }
}

struct Prepared {
    args: Args,
    id: String,
    child: Ctx,
    agent: Agent,
    model_notes: Vec<String>,
    instructions: Vec<String>,
}

impl Subagent {
    async fn prepare(&self, args: Args, ctx: &Ctx) -> Result<Prepared, ToolError> {
        let id = format!(
            "{}-subagent-{}",
            ctx.spill_namespace(),
            uuid::Uuid::new_v4().simple()
        );
        // Tree, locks and renumbering are shared with the parent (same files);
        // transcript, spill name and token are the child's own.
        let mut child = ctx
            .clone()
            .with_cancel(ctx.cancel.child_token())
            .with_session(&id, ctx.spill_root())
            .with_own_writes();
        if args.background {
            // Reads in another context must not acknowledge changes for the caller.
            child.viewed = Default::default();
        }

        let mut agent = self.agent.as_ref().clone();
        let mut model_notes = Vec::new();
        if let Some(tools) = &self.tools {
            let patch = Arc::make_mut(&mut agent.brief);
            patch.registry = tools(&child, std::mem::take(&mut patch.registry));
        }
        if let Some(name) = &args.model {
            if name.trim().is_empty() {
                return Err(ToolError::Invalid(
                    "subagent model must not be empty".into(),
                ));
            }
            let resolve = self.models.as_ref().ok_or_else(|| {
                ToolError::Invalid("model selection is not available for this subagent".into())
            })?;
            let connection = resolve(name)?;
            model_notes = connection.notes;
            agent.retarget(connection.transport, connection.spec);
        }
        if let Some(names) = &args.tools {
            let patch = Arc::make_mut(&mut agent.brief);
            for name in names {
                if patch.registry.get(name).is_none() {
                    return Err(ToolError::Invalid(format!(
                        "subagent tool `{name}` is unavailable"
                    )));
                }
            }
            patch.registry = std::mem::take(&mut patch.registry).only(names);
        }
        let instructions = if args.skills.is_empty() {
            Vec::new()
        } else {
            let shelf = self.skills.as_ref().ok_or_else(|| {
                ToolError::Invalid("skill preloading is not available for this subagent".into())
            })?;
            shelf.preload(&args.skills).await?
        };

        Ok(Prepared {
            args,
            id,
            child,
            agent,
            model_notes,
            instructions,
        })
    }
}

impl Prepared {
    async fn run(
        self,
        archive: &dyn Archive,
        retry: &Retry,
        ctx: &Ctx,
    ) -> Result<Report, ToolError> {
        let Self {
            args,
            id,
            child,
            agent,
            mut model_notes,
            instructions,
        } = self;
        let timeout = tool::limit::timeout(
            args.timeout_ms.map(|ms| ms.get()),
            tool::limit::DEFAULT_CALL,
            tool::limit::MAX_CALL,
        );
        let stop = child.cancel.clone();
        let _stop = stop.clone().drop_guard();
        let expires = tokio::time::Instant::now() + timeout;
        let (tx, mut rx) = agent::event::channel();
        let caller = ctx.clone();
        let heard = tokio::spawn(async move {
            let mut heard = Heard::default();
            let mut said = String::new();
            tell(&caller, &heard, &mut said);
            loop {
                // A held call is shown when its hold ends, even if the child
                // falls silent then, as it does under a long command.
                let event = match heard.doing.due() {
                    Some(due) => tokio::select! {
                        event = rx.recv() => event,
                        _ = tokio::time::sleep_until(due.into()) => {
                            heard.doing.settle(Instant::now());
                            tell(&caller, &heard, &mut said);
                            continue;
                        }
                    },
                    None => rx.recv().await,
                };
                let Some(event) = event else { break };
                let now = Instant::now();
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
                    Event::Warning(warning) => heard.warnings.push(warning),
                    Event::TurnEnd { usage, .. } => {
                        heard.spent.add(&usage);
                        heard.turn = Default::default();
                    }
                    Event::ToolQueued { name, args, .. } => {
                        heard.doing.want(call_line(&name, &args), now)
                    }
                    _ => {}
                }
                heard.doing.settle(now);
                tell(&caller, &heard, &mut said);
            }
            heard
        });
        // One budget for the whole child: at `expires` its stop token trips,
        // the same token a parent stop uses.
        let budget_stop = stop.clone();
        let timed_out = Arc::new(AtomicBool::new(false));
        let expired = timed_out.clone();
        let budget = tokio::spawn(async move {
            tokio::time::sleep_until(expires).await;
            expired.store(true, Ordering::Relaxed);
            budget_stop.cancel();
        });
        // Keep selected instructions with the task: transient notes may be
        // omitted by compaction before the child has finished using them.
        let prompt = if instructions.is_empty() {
            args.prompt
        } else {
            format!(
                "Follow these preloaded skill instructions for the task below.\n\n{}\n\n---\nTask:\n{}",
                instructions.join("\n\n---\n\n"),
                args.prompt,
            )
        };
        let mut session = Session::with_prompt(prompt);
        // One span for the whole child, so the log can file its records
        // under it rather than lose them among its siblings'.
        let child_span = tracing::info_span!(
            target: "pi::subagent",
            "subagent",
            session = %child.spill_namespace(),
            description = %args.description,
        );
        // Use the loop's event entrance so a failed or cancelled request still
        // leaves the connection diagnostics in the journal.
        async {
            let reporter = agent::event::Reporter::new(tx.clone(), agent.brief.hooks.clone());
            for note in &model_notes {
                reporter.warn(note.clone()).await;
            }
        }
        .instrument(child_span.clone())
        .await;
        // The token ends the run — the budget or a stop — unwinding like a
        // stop; one ignoring it is dropped after STOP_GRACE.
        let checkpoint = Checkpoint {
            archive,
            parent: ctx.spill_namespace(),
            id: &id,
            model: &agent.model.spec.model,
        };
        let ran = {
            let mut run = std::pin::pin!(
                agent
                    .run_with_input(&mut session, &child, &tx, retry, &checkpoint)
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
        budget.abort();
        stop.cancel();
        // The collector ends when the last sender goes, and `run` held one.
        drop(tx);
        let mut lost = false;
        let mut heard = match heard.await {
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
        heard.spent.add(&heard.turn);
        for warning in &heard.warnings {
            if !model_notes.contains(warning) {
                model_notes.push(warning.clone());
            }
        }

        // A child dropped past its grace never reached its own close.
        session.close_dangling();
        archive.keep_model(ctx.spill_namespace(), &id, &agent.model.spec.model, session);

        let mut outcome = ExecutionOutcome::Succeeded;
        let cut = match ran {
            Ok(_) => None,
            // A stop only: our own token tripping leaves the parent's alone —
            // conflating them would end the caller's whole turn over a wedged child.
            // A child that outlived its grace was stopped all the same.
            Err(AgentError::Cancelled | AgentError::Unstopped) if ctx.cancel.is_cancelled() => {
                if !args.background {
                    return Err(ToolError::Cancelled);
                }
                outcome = ExecutionOutcome::Cancelled;
                Some("execution cancelled".into())
            }
            // Our token trips only for the parent's stop, caught just above,
            // or for the budget.
            Err(AgentError::Cancelled | AgentError::Unstopped)
                if timed_out.load(Ordering::Relaxed) =>
            {
                Some(format!(
                    "reached the total runtime budget of {}",
                    llm::figures::elapsed(timeout)
                ))
            }
            Err(why) => {
                if !args.background {
                    // Error results cannot carry `.with_spent` into the caller's run.
                    let uncounted = if heard.spent.input + heard.spent.output > 0 {
                        format!(
                            " ({} turn(s), {}, ran uncounted)",
                            heard.turns,
                            llm::figures::slash(heard.spent.input, heard.spent.output)
                        )
                    } else {
                        String::new()
                    };
                    return Err(ToolError::Invalid(with_model_notes(
                        format!("subagent: {why}{uncounted}"),
                        &model_notes,
                    )));
                }
                outcome = ExecutionOutcome::Failed;
                Some(format!("subagent: {why}"))
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
            Some(_) if ctx.cancel.is_cancelled() => None,
            Some(command) => {
                // The workspace's own cap, not a shorter one: a subagent's
                // leash isn't a request for a shorter suite.
                let room = tool::limit::MAX_CALL;
                // The caller's context, not the child's: a child stopped by
                // its own cap leaves that token tripped, and this cancelled.
                let outcome = match bash::run(command, ctx.workspace.root(), Some(room), ctx).await
                {
                    Ok(ran) => Outcome::Ran {
                        code: ran.code,
                        body: ran.body,
                    },
                    // A stop ends the caller's turn as elsewhere; anything else is an
                    // outcome, not a reason to drop the work the child already did.
                    Err(ToolError::Cancelled) if args.background => {
                        outcome = ExecutionOutcome::Cancelled;
                        Outcome::NoVerdict("execution cancelled".into())
                    }
                    Err(ToolError::Cancelled) => return Err(ToolError::Cancelled),
                    Err(ToolError::Timeout { ms, .. }) => Outcome::CutOff { ms },
                    Err(why) => Outcome::NoVerdict(why.to_string()),
                };
                Some(Checked {
                    // Flattened, since it is about to be quoted into one line.
                    command: command.replace('\n', " "),
                    outcome,
                })
            }
        };
        // Synchronous calls fold this usage into their caller's run. Background
        // reports retain it as metadata; the shared meter already counted it.
        if outcome == ExecutionOutcome::Succeeded
            && let Some(checked) = &check
        {
            outcome = match &checked.outcome {
                Outcome::Ran { code: 0, .. } => ExecutionOutcome::Succeeded,
                Outcome::Ran { .. } => ExecutionOutcome::Failed,
                Outcome::CutOff { .. } | Outcome::NoVerdict(_) => ExecutionOutcome::Incomplete,
            };
        }
        let unfinished = cut.is_some();
        let output = ToolOutput::text(with_model_notes(
            answer(&heard, cut.as_deref(), lost, &wrote, check.as_ref()),
            &model_notes,
        ))
        .with_preview(sketch(&args.description, &heard))
        .with_spent(heard.spent);
        if unfinished && outcome == ExecutionOutcome::Succeeded {
            outcome = ExecutionOutcome::Incomplete;
        }
        Ok(Report {
            outcome,
            output: if outcome != ExecutionOutcome::Succeeded {
                output.warned()
            } else {
                output
            },
        })
    }
}

fn with_model_notes(body: String, notes: &[String]) -> String {
    if notes.is_empty() {
        body
    } else {
        format!("{}\n\n{body}", notes.join("\n"))
    }
}

// The finished-call line: task, then its cost in brackets. The task leads
// because several children run at once, so a bare cost names none of them.
fn sketch(description: &str, heard: &Heard) -> String {
    let spent = llm::figures::slash(heard.spent.input, heard.spent.output);
    // Flattened, not trusted: this is a single line, and a newline in it
    // would break the line in two.
    let task = description.replace('\n', " ");
    match task.trim() {
        // Nothing to hold apart from, so the brackets would enclose the
        // whole line rather than rank it.
        "" => spent,
        task => format!("{task} [{spent}]"),
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
    let take = llm::figures::shown(n, NAMED);
    let named = wrote
        .iter()
        .take(take)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match n - take {
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
            let took = llm::figures::elapsed(Duration::from_millis(*ms));
            format!("[verify `{command}`: no verdict — killed after {took}, having run that long]")
        }
        Outcome::NoVerdict(why) => format!("[verify `{command}`: no verdict — {why}]"),
    }
}

struct Checkpoint<'a> {
    archive: &'a dyn Archive,
    parent: &'a str,
    id: &'a str,
    model: &'a str,
}

impl agent::seams::Input for Checkpoint<'_> {
    fn draft(
        &self,
        draft: agent::session::Draft,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.archive.draft(self.parent, self.id, draft)
    }
    fn take(&self) -> Option<agent::session::Prompt> {
        None
    }
    fn checkpoint(
        &self,
        session: &Session,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.archive
            .checkpoint(self.parent, self.id, self.model, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_inside_the_hold_waits_for_it_to_pass() {
        let start = Instant::now();
        let mut doing = Doing::default();
        doing.want("read a".into(), start);
        doing.want("read b".into(), start + HOLD / 2);
        assert_eq!(doing.shown, "read a");
        assert_eq!(doing.due(), Some(start + HOLD));
        doing.want("read c".into(), start + HOLD / 2);
        doing.settle(start + HOLD);
        assert_eq!(doing.shown, "read c", "the newest waiting call wins");
    }
}
