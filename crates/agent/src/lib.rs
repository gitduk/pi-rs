use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use llm::message::{Message, ToolCall, ToolResult};
use llm::model::ModelSpec;
use llm::request::{Effort, Request};
use llm::stream::{Accumulator, InvalidToolArgs, StreamEvent, Usage};
use llm::transport::Transport;
use tokio::sync::mpsc::UnboundedSender;
use tool::{Concurrency, Ctx, Registry, ToolError, ToolOutput};
use tracing::Instrument as _;

use crate::session::Session;

pub mod context;
pub mod event;
pub mod ext;
pub mod seams;
pub mod session;

use event::say;
pub use event::{Event, Totals};
pub use ext::approval::Ceiling;
pub use ext::compact::{Policy, Report};
pub use ext::compactor::Summarizing;
pub use ext::retry::Retry;
pub use seams::{Approver, Compactor, Decision, Fitted, Home, Steer, Untouched, Working};

pub const DEFAULT_SYSTEM: &str = include_str!("../prompts/system.md");

// A tool that fails twice in a row is named. One failure is ordinary — the
// model reads the error and tries again; the second tells it nothing the
// first did not, so there is no leeway the way there is for a re-read.
const FAILURE_LIMIT: usize = 2;
// Headroom for framing the estimate does not model. Compacting slightly early
// costs a little quality; compacting late costs the whole turn.
const SAFETY_MARGIN: usize = 2_000;

// How hard to squeeze after the provider says the request did not fit. Our
// estimate was wrong by an unknown amount, so the correction is blunt.
const SQUEEZE: f64 = 0.6;

// Attempts to shrink one turn before giving up on it.
const MAX_SQUEEZE: usize = 3;

// How much of a failed argument blob rides back to the model and the log.
// Longer blobs show a window around serde's column, not the head: the parse
// fails where the text stopped, and that is usually the tail.
const MAX_INVALID_ARGS_SHOWN: usize = 400;

/// How long a run has to wind down after its stop token is tripped before it
/// is dropped where it stands.
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Brain(#[from] llm::BrainError),

    #[error("cancelled")]
    Cancelled,

    #[error("still running {}s after cancel", STOP_GRACE.as_secs())]
    Unstopped,
}

/// The wire a lane talks on, and the model on the other end. Swapped whole by
/// `/model`, which moves nothing else about a run.
#[derive(Clone)]
pub struct Model {
    pub transport: Arc<dyn Transport>,
    pub spec: ModelSpec,
}

/// What a run is allowed to do and what it is told. Rebuilt whole by `/reload`
/// and by an opened checkout; a subagent derives its own from its caller's.
#[derive(Clone)]
pub struct Briefing {
    pub registry: Registry,
    pub system: String,
    pub effort: Effort,
    pub approver: Arc<dyn Approver>,
    /// How long a subagent may run silent before it is read as wedged.
    /// None defaults to 1800 s.
    pub subagent_deadline: Option<std::time::Duration>,
}

#[derive(Clone)]
pub struct Agent {
    pub model: Arc<Model>,
    pub brief: Arc<Briefing>,
    /// What shrinks the transcript when it outgrows the window. The identity
    /// until something is installed — see `Compactor`.
    pub compactor: Arc<dyn Compactor>,
}

// Per-tool failure streaks across one run, so a loop can be named. Keyed by
// tool and the stable code its error carries: an edit that keeps coming back
// "would not parse" is one loop whatever the prose says, while a genuinely
// different error starts a new count.
type Failures = HashMap<(String, String), usize>;

// What a streamed call resolves to before anything runs. Deciding first keeps
// the result list aligned with the call list even when nothing executes.
enum Action {
    Reject(String),
    Run(Arc<dyn tool::Tool>),
}
impl Agent {
    /// An agent with no tools: whoever assembles it installs a registry on
    /// the brief, as the identity compactor waits for a real one.
    pub fn new(transport: Arc<dyn Transport>, spec: ModelSpec) -> Self {
        Self {
            model: Arc::new(Model { transport, spec }),
            brief: Arc::new(Briefing {
                registry: Registry::new(),
                system: DEFAULT_SYSTEM.to_string(),
                effort: Effort::Off,
                approver: Arc::new(Ceiling(tool::Tier::Exec)),
                subagent_deadline: None,
            }),
            compactor: Arc::new(seams::Untouched),
        }
    }

    /// The model this agent runs: the wire and the facts about it.
    pub fn spec(&self) -> &ModelSpec {
        &self.model.spec
    }

    /// Point the same run at a different host, and the budget at that host's.
    pub fn retarget(&mut self, transport: Arc<dyn Transport>, spec: ModelSpec) {
        self.model = Arc::new(Model { transport, spec });
    }

    /// Put what the config and the workspace decided onto this agent: the
    /// tools, the ceiling, the system prompt, the effort — as one value.
    ///
    /// Swapped whole rather than written field by field: a run in flight keeps
    /// the brief it started on, and a reader sees one or the other, never half
    /// of each. A caller that wants a subagent hangs it on the brief first —
    /// see `cli/src/core/subagent.rs`.
    pub fn apply(&mut self, brief: Arc<Briefing>) {
        self.brief = brief;
    }

    /// A run nobody is talking to. What a subagent, a one-shot and a test all
    /// want: named for what it is rather than passed an empty mailbox at every
    /// call site.
    ///
    /// `retry` is the schedule for a request the provider could not serve. Read
    /// where the run starts rather than kept on the agent, so a `/reload` that
    /// changed it reaches the next run without anything having to refresh a
    /// copy.
    pub async fn run(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        retry: &Retry,
    ) -> Result<Usage, AgentError> {
        self.steered(session, ctx, tx, &Steer::default(), retry)
            .await
    }

    /// The same run, with somewhere for the user to speak into while it works.
    pub async fn steered(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        steer: &Steer,
        retry: &Retry,
    ) -> Result<Usage, AgentError> {
        let mut totals = Usage::default();
        // How many times a tool has failed in a row, so a loop can be named —
        // naming it is the only thing that stops one.
        let mut failures: Failures = Failures::new();

        // Our token estimate is a bound, not a measurement. When the provider
        // says otherwise, this is what carries the correction forward.
        let mut scale = 1.0f64;
        // A window the provider named, which outranks whatever the spec says.
        let mut hard: Option<usize> = None;
        // Across the whole run, not the turn: a status line that reset this
        // every turn would report "not compacted" for a run that just was.
        let mut compactions = 0usize;

        for turn in 1.. {
            // What was said while the run worked. Here and nowhere else: a
            // `tool_use` must be answered by its results before anything else
            // may speak, and this is the first point where all of them are.
            for said in steer.take() {
                session.prompt(said);
            }
            say(tx, Event::TurnStart { turn });
            // Entered around each await rather than held across them: a guard
            // spanning an await point labels whatever else the runtime polls.
            // A run and its subagents share one journal, and the spans of
            // parallel children are indistinguishable by name alone.
            let span = tracing::info_span!(
                target: "pi::loop",
                "turn",
                turn,
                session = %ctx.spill_namespace(),
            );
            let mut squeezes = 0usize;
            // Kept past the retry loop: the fallback below prices what was
            // actually sent, which a squeeze or a compaction may have changed.
            let mut sent;
            // Kept for the same reason the transcript is: what the status line
            // reports as the window's state has to be the request that ran.
            let mut budget;
            let mut used;

            let done = loop {
                budget = ((hard.unwrap_or_else(|| self.budget()) as f64) * scale) as usize;
                let fitted = self
                    .compactor
                    .compact(session, self.working(), budget, squeezes > 0, tx)
                    .instrument(span.clone())
                    .await;
                totals.add(&fitted.spent);
                sent = fitted.context;
                if fitted.changed {
                    compactions += 1;
                }
                used = llm::estimate::tokens(&sent, &self.model.spec);
                say(tx, Event::Context { used, budget });
                tracing::debug!(
                    target: "pi::loop",
                    parent: &span,
                    messages = sent.len(),
                    estimated = used,
                    budget,
                    squeezes,
                    scale,
                    hard = hard.unwrap_or(0),
                    effort = ?self.brief.effort,
                    "sending"
                );
                let req = Request {
                    system: Some(self.brief.system.clone()),
                    messages: sent,
                    tools: self.brief.registry.defs(),
                    max_output_tokens: None,
                    temperature: None,
                    effort: self.brief.effort,
                    tool_choice: Default::default(),
                };

                match self
                    .stream_turn(&req, ctx, tx, retry)
                    .instrument(span.clone())
                    .await
                {
                    Ok(done) => break done,
                    Err((AgentError::Brain(e), _))
                        if llm::classify(&e) == llm::Fault::Overflow && squeezes < MAX_SQUEEZE =>
                    {
                        squeezes += 1;
                        // The refusal usually names the real window. Reading it
                        // beats guessing when the estimate was wrong by an
                        // unknown amount.
                        match llm::fault::overflow_limit(&e) {
                            Some(limit) => {
                                // Every shrink so far was guesswork against the
                                // window the spec claimed, and that baseline is
                                // now replaced; corrections learned after this
                                // still stack on top.
                                if hard.is_none() {
                                    scale = 1.0;
                                }
                                hard = Some(self.budget_within(limit));
                                say(
                                    tx,
                                    Event::Warning(format!(
                                        "the provider reports a {limit}-token window; refitting to it"
                                    )),
                                );
                            }
                            None => {
                                scale *= SQUEEZE;
                                say(
                                    tx,
                                    Event::Warning(format!(
                                        "the request did not fit and named no limit; \
                                     retrying at {}% of the estimated budget",
                                        (scale * 100.0).round()
                                    )),
                                );
                            }
                        }
                    }
                    Err((AgentError::Cancelled, Some(partial))) => {
                        let Message::Assistant { content, .. } = partial.message else {
                            unreachable!("the accumulator only ever builds an assistant message")
                        };
                        if !content.is_empty() {
                            session.push_assistant(content);
                        }
                        return Err(AgentError::Cancelled);
                    }
                    Err((err, _)) => return Err(err),
                }
            };

            totals.add(&done.usage);
            say(tx, Event::TurnEnd { usage: done.usage });

            // Two providers accept an oversized request instead of refusing it:
            // one silently, one by truncating and then having no room to answer.
            // Both look like success and neither can be caught before the fact.
            let window = self.model.spec.context_window as usize;
            let silently_truncated = done.usage.input as usize > window
                || (done.stop == llm::StopReason::MaxTokens && done.usage.output == 0);
            if silently_truncated && scale > SQUEEZE.powi(MAX_SQUEEZE as i32) {
                scale *= SQUEEZE;
                say(
                    tx,
                    Event::Warning(format!(
                        "the provider took {} input tokens against a {window}-token window and \
                     answered from a truncated prompt; tightening the budget",
                        done.usage.input
                    )),
                );
            }

            tracing::debug!(
                target: "pi::loop",
                parent: &span,
                stop = ?done.stop,
                calls = done.message.tool_calls().count(),
                invalid = done.invalid.len(),
                "replied"
            );

            let calls: Vec<ToolCall> = done.message.tool_calls().cloned().collect();
            let Message::Assistant { content, .. } = done.message else {
                unreachable!("the accumulator only ever builds an assistant message")
            };
            session.push_assistant(content);

            if calls.is_empty() {
                // The model stopped, but the user spoke while it was speaking.
                // Ending here would post `Done` and leave the line to start a
                // second run saying what this one can still hear.
                if !steer.is_empty() {
                    continue;
                }
                say(
                    tx,
                    Event::Done {
                        turns: turn,
                        usage: totals,
                        // Re-measured rather than reused: `used` is what went
                        // out, and the reply landed in the session since.
                        ctx: (
                            llm::estimate::tokens(&session.context(), &self.model.spec),
                            budget,
                        ),
                        compactions,
                    },
                );
                return Ok(totals);
            }

            let bad: HashMap<_, _> = done
                .invalid
                .iter()
                .map(|i| (i.call.clone(), i.clone()))
                .collect();
            let results = self
                .run_calls(&calls, &bad, ctx, tx, &mut failures, &mut totals)
                .instrument(span.clone())
                .await?;
            let ids = session.push_previewed(results);
            // A state update, not a drawing instruction: the renderer derives
            // the results' rows from these entries through the A table.
            say(
                tx,
                Event::Committed {
                    entries: session.entries_for(&ids).into_iter().cloned().collect(),
                },
            );
        }

        unreachable!("an unlimited run can only leave by returning inside the loop")
    }

    /// What a manual compaction leaves alone, against this agent's window: the
    /// compactor's answer to how much of the end it will not touch. The surface
    /// says so to whoever asked for a compaction.
    pub fn kept_tokens(&self) -> usize {
        self.compactor.kept_tokens(self.budget())
    }

    /// Compact now, at the user's word rather than the window's.
    ///
    /// The target is the tail the agent is working from — the same number the
    /// automatic pass protects — so this means "summarize everything but what I
    /// am in the middle of". Unlike the automatic pass it runs even when the
    /// transcript already fits: the point is that the user knows a phase has
    /// ended, which no budget can tell.
    ///
    /// Asked of the compactor with the rest of what a pass needs; a compactor
    /// that does nothing answers `None`.
    pub async fn compact_now(
        &self,
        session: &mut Session,
        focus: Option<&str>,
    ) -> Option<(Report, Usage)> {
        self.compactor
            .compact_now(session, self.working(), self.budget(), focus)
            .await
    }

    /// The model doing the work and the wire it goes out on, for whatever needs
    /// both: the compactor writes its summary with them when it has none of its
    /// own.
    fn working(&self) -> Working<'_> {
        Working {
            transport: &*self.model.transport,
            spec: &self.model.spec,
        }
    }

    /// What the transcript may occupy. The reply, the system prompt and the
    /// tool schemas all share the window with it, so each is subtracted before
    /// the transcript gets to claim what is left.
    pub fn budget(&self) -> usize {
        self.budget_within(self.model.spec.context_window as usize)
    }

    // The same accounting against a window the provider named instead of the
    // one the spec claims.
    fn budget_within(&self, window: usize) -> usize {
        // A spec may declare an output cap larger than the window it is being
        // used against — an overridden window, a proxy, a stale entry. Reserving
        // it verbatim would leave the transcript nothing at all.
        let reply = (self.model.spec.max_output_tokens as usize).min(window / 4);
        let fixed = llm::estimate::text(&self.brief.system)
            + llm::estimate::tool_defs(&self.brief.registry.defs())
            + reply
            + SAFETY_MARGIN;
        // Even an unworkable configuration leaves a floor: stripping the
        // transcript to nothing helps no one.
        window.saturating_sub(fixed).max(window / 4)
    }

    // Run one request, retrying while the provider says it is a passing problem.
    #[allow(
        clippy::result_large_err,
        reason = "Ok carries a Completion too; boxing Err would not shrink the Result"
    )]
    async fn stream_turn(
        &self,
        req: &Request,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        retry: &Retry,
    ) -> Result<llm::stream::Completion, (AgentError, Option<llm::stream::Completion>)> {
        let mut attempt = 0usize;
        loop {
            let (err, partial) = match self.attempt(req, ctx, tx, retry).await {
                Ok(done) => return Ok(done),
                Err(err) => err,
            };
            if matches!(err, AgentError::Cancelled) {
                return Err((err, partial));
            }
            let e = match err {
                AgentError::Brain(e) => e,
                other => return Err((other, partial)),
            };

            if attempt >= retry.attempts || llm::classify(&e) != llm::Fault::Transient {
                // The classification, not just the error: "why was this not
                // retried" is answerable from the fault and from nothing else.
                tracing::error!(
                    target: "pi::wire",
                    attempts = attempt,
                    fault = ?llm::classify(&e),
                    error = %e,
                    "giving up"
                );
                return Err((AgentError::Brain(e), partial));
            }

            attempt += 1;
            let delay = retry.delay(attempt);
            say(
                tx,
                Event::Retrying {
                    attempt,
                    delay_ms: delay.as_millis() as u64,
                    reason: e.to_string(),
                },
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = ctx.cancel.cancelled() => return Err((AgentError::Cancelled, None)),
            }
        }
    }

    // One attempt. Deltas reach the renderer as they arrive, so a retry shows
    // as a false start — which the Retrying event is there to explain.
    #[allow(
        clippy::result_large_err,
        reason = "Ok carries a Completion too; boxing Err would not shrink the Result"
    )]
    async fn attempt(
        &self,
        req: &Request,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        retry: &Retry,
    ) -> Result<llm::stream::Completion, (AgentError, Option<llm::stream::Completion>)> {
        // A fresh accumulator per attempt: half a stream must not bleed into
        // the message the retry produces.
        let mut acc = Accumulator::new(self.model.spec.model.clone());
        let idle = retry.idle;

        let mut stream = tokio::select! {
            r = leashed(idle, self.model.transport.stream(&self.model.spec, req)) => match r {
                Ok(r) => r.map_err(|e| (AgentError::from(e), None))?,
                Err(e) => return Err((AgentError::Brain(e), None)),
            },
            _ = ctx.cancel.cancelled() => return Err((AgentError::Cancelled, None)),
        };

        loop {
            let next = tokio::select! {
                n = leashed(idle, stream.next()) => match n {
                    Ok(n) => n,
                    // A provider that stops sending mid-stream would otherwise
                    // hold the turn open until the user gives up.
                    Err(e) => return Err((AgentError::Brain(e), None)),
                },
                _ = ctx.cancel.cancelled() => {
                    return Err((AgentError::Cancelled, Some(acc.finish())));
                }
            };
            let Some(ev) = next else { break };
            let ev = match ev {
                Ok(ev) => ev,
                Err(e) => return Err((AgentError::from(e), None)),
            };
            match &ev {
                StreamEvent::TextDelta { delta, .. } => {
                    say(tx, Event::TextDelta(delta.clone()));
                }
                StreamEvent::ReasoningDelta { delta, .. } => {
                    say(tx, Event::ReasoningDelta(delta.clone()));
                }
                // The one measured number that arrives before the answer does.
                StreamEvent::MessageStart { usage, .. } => {
                    say(tx, Event::Usage(*usage));
                }
                _ => {}
            }
            acc.push(ev);
        }

        // What the host owed and did not send. Said once per session by the
        // reporter itself, and said here rather than only in the journal: a
        // turn that quietly came back smaller looks exactly like an ordinary
        // one, which is the whole reason it needs saying.
        for gap in self.model.transport.gaps() {
            say(tx, Event::Warning(gap));
        }

        Ok(acc.finish())
    }

    // Every call gets exactly one result, in call order: an unanswered
    // `tool_use` makes the next request invalid on both wires.
    //
    // `spent` is where a nested call's costs land — a subagent's whole run —
    // so the run that called it reports them.
    async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<String, InvalidToolArgs>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        failures: &mut Failures,
        spent: &mut Usage,
    ) -> Result<Vec<(ToolResult, Option<String>)>, AgentError> {
        // Read once for the batch rather than per failure, and from `ctx`
        // rather than the machine: a session moves — `/new`, `/resume` — and
        // the context is what moves with it.
        let journal = ctx
            .session()
            .and_then(|id| tool::state::session_dir(ctx.workspace.root(), id))
            .map(|d| d.join(tool::state::JOURNAL_FILE));
        let journal = journal.as_deref();
        let actions: Vec<Action> = calls
            .iter()
            .map(|c| {
                if let Some(invalid) = bad.get(&c.id) {
                    let snippet = invalid_args_snippet(&invalid.raw, &invalid.error);
                    tracing::warn!(
                        target: "pi::wire",
                        call = %c.id,
                        name = %c.name,
                        error = %invalid.error,
                        raw = %snippet,
                        "tool arguments were not valid JSON"
                    );
                    return Action::Reject(format!(
                        "arguments were not valid JSON ({}); you sent: {snippet}; \
                         send the whole object again",
                        invalid.error,
                    ));
                }
                let Some(tool) = self.brief.registry.get(&c.name) else {
                    return Action::Reject(format!(
                        "no tool named `{}`; available: {}",
                        c.name,
                        self.brief.registry.names().join(", ")
                    ));
                };
                match self.brief.approver.approve(&c.name, tool.tier(), &c.args) {
                    Decision::Allow => Action::Run(tool),
                    Decision::Deny(why) => Action::Reject(why),
                }
            })
            .collect();

        for (call, action) in calls.iter().zip(&actions) {
            match action {
                Action::Reject(why) => {
                    say(
                        tx,
                        Event::ToolDenied {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            reason: why.clone(),
                        },
                    );
                }
                Action::Run(_) => {
                    say(
                        tx,
                        Event::ToolStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: call.args.clone(),
                        },
                    );
                }
            }
        }

        let exclusive = actions
            .iter()
            .any(|a| matches!(a, Action::Run(t) if t.concurrency() == Concurrency::Exclusive));

        // One future per call, awaited positionally: results stay aligned with
        // the calls; an Exclusive batch runs in turn, a Shared batch joins.
        let futures: Vec<_> = calls
            .iter()
            .zip(&actions)
            .map(|(call, action)| {
                async move {
                    match action {
                        Action::Reject(_) => None,
                        Action::Run(t) => {
                            Some(tool::output::gated(t.as_ref(), call.args.clone(), ctx).await)
                        }
                    }
                }
                .instrument(ran(call))
            })
            .collect();
        let outputs: Vec<Option<Result<ToolOutput, ToolError>>> = if exclusive {
            let mut outputs = Vec::with_capacity(futures.len());
            for f in futures {
                outputs.push(f.await);
            }
            outputs
        } else {
            futures::future::join_all(futures).await
        };

        let mut results = Vec::with_capacity(calls.len());
        for ((call, action), output) in calls.iter().zip(&actions).zip(outputs) {
            // The copy the screen drew for this result, sent with it so a
            // rebuild draws those bytes rather than reading the content again.
            let mut preview = None;
            let result = match (action, output) {
                (Action::Reject(why), _) => failed(call, why.clone(), None, failures, journal),
                (_, Some(Err(ToolError::Cancelled))) => return Err(AgentError::Cancelled),
                (_, Some(Err(e))) => {
                    let mut body = e.to_string();
                    if let Some(code) = e.code() {
                        // A stable code lets the model branch on what happened
                        // instead of parsing the prose; the prose still leads.
                        body = format!("Error: {body} [code: {code}]");
                    }
                    // The pending live line draws from this same text, so
                    // adoption's equality check gets both halves from one
                    // source; without it a multi-line error renders one way
                    // live and another after a rebuild.
                    preview = Some(body.clone());
                    say(
                        tx,
                        Event::ToolEnd {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            is_error: true,
                            preview: body.clone(),
                        },
                    );
                    failed(call, body, e.category(), failures, journal)
                }
                (_, Some(Ok(out))) => {
                    // A nested run's spend belongs to the run that called it:
                    // folded in here, it reaches `Event::Done` and the return.
                    spent.add(&out.spent);
                    preview = out.preview.clone();
                    say(
                        tx,
                        Event::ToolEnd {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            is_error: false,
                            preview: out.preview(),
                        },
                    );
                    note_success(call, failures);
                    ToolResult {
                        call: call.id.clone(),
                        name: call.name.clone(),
                        content: out.content,
                        is_error: false,
                        useless: out.useless,
                    }
                }
                (_, None) => unreachable!("only rejected calls produce no output"),
            };
            results.push((result, preview));
        }

        Ok(results)
    }
}

// A success resets the failure streak for this tool — the loop-breaker only
// names an unbroken run of failures — except for edit, whose every success is
// a different file: landing one edit does not mean the next will land, and a
// call that keeps coming back malformed must keep being counted until the model
// actually changes approach.
fn note_success(call: &ToolCall, failures: &mut Failures) {
    if call.name != "edit" {
        failures.retain(|(name, _), _| name != &call.name);
    }
}

fn ran(call: &ToolCall) -> tracing::Span {
    tracing::info_span!(target: "pi::tool", "tool", name = %call.name, call = %call.id)
}

// One failed argument blob as shown to the model and the journal: the whole
// text when it fits, else a window around serde's column. serde numbers the
// column from 1 over the trimmed bytes; windowing in chars is close enough,
// and an error that names no column falls back to the tail.
fn invalid_args_snippet(raw: &str, err: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() <= MAX_INVALID_ARGS_SHOWN {
        return raw.to_string();
    }
    let column = err
        .rsplit_once("column ")
        .and_then(|(_, n)| n.trim().parse::<usize>().ok())
        .unwrap_or(chars.len())
        .saturating_sub(1)
        .min(chars.len());
    let half = MAX_INVALID_ARGS_SHOWN / 2;
    let to = (column.saturating_sub(half) + MAX_INVALID_ARGS_SHOWN).min(chars.len());
    let from = to - MAX_INVALID_ARGS_SHOWN;

    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.extend(chars[from..to].iter());
    if to < chars.len() {
        out.push('…');
    }
    out
}

// A call that did not run, or ran and failed.
//
// The notice goes inside the error body rather than beside it: a failure has
// no content blocks to append one to, and the model reads the body.
fn failed(
    call: &ToolCall,
    mut body: String,
    code: Option<&'static str>,
    failures: &mut Failures,
    journal: Option<&std::path::Path>,
) -> ToolResult {
    if let Some(notice) = too_many_failures(call, code, failures, journal) {
        body.push_str(&notice);
    }
    ToolResult::error(call.id.clone(), &call.name, body)
}

// Name a tool whose failures are piling up. The count is per tool and per
// stable error code, so the wording of the refusal — which a loop keeps
// changing — never matters: a call that keeps coming back refused the same way
// is a loop, whatever the prose says, while a genuinely different error
// starts a new count. Two failures is already the whole story; the second
// tells the model nothing the first did not, so there is no leeway the way
// there is for a re-read. Naming resets the count, so a mistake made long
// after the loop was broken is not called the Nth repeat of it.
fn too_many_failures(
    call: &ToolCall,
    code: Option<&'static str>,
    failures: &mut Failures,
    journal: Option<&std::path::Path>,
) -> Option<String> {
    let key = (
        call.name.clone(),
        code.map(str::to_owned).unwrap_or_default(),
    );
    let n = failures.entry(key).or_insert(0);
    *n += 1;
    if *n < FAILURE_LIMIT {
        return None;
    }
    let seen = *n;
    *n = 0;
    tracing::warn!(
        target: "pi::tool",
        tool = %call.name,
        code = code.unwrap_or_default(),
        seen,
        "a tool keeps failing in a row"
    );
    let mut notice = format!(
        "\n[the same `{}` call has now failed the same way {seen} times. \
         Sending it again will not change the answer — change the call, \
         or reach the goal another way.",
        call.name
    );
    // The journal holds what the transcript cannot: the call as it went out on
    // the wire. Named exactly, because it is this session's and `ctx` followed
    // the session here.
    if let Some(journal) = journal {
        notice.push_str(&format!(
            " The wire records for this session are in {} — it is JSONL, so \
             grep it rather than reading it whole.",
            journal.display()
        ));
    }
    notice.push(']');
    Some(notice)
}

fn wedged(idle: std::time::Duration) -> llm::BrainError {
    llm::BrainError::Stream(format!("the stream sent nothing for {}s", idle.as_secs()))
}

// The leash every provider call here keeps: silence past `idle` reads as a
// wedged stream, whether the call drives a turn or a compaction.
async fn leashed<T>(
    idle: std::time::Duration,
    fut: impl std::future::Future<Output = T>,
) -> llm::Result<T> {
    tokio::time::timeout(idle, fut)
        .await
        .map_err(|_| wedged(idle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::message::ToolCall;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("call_{name}"),
            name: name.into(),
            args: serde_json::json!({}),
        }
    }

    #[test]
    fn two_same_code_failures_are_named() {
        let mut f = Failures::new();
        assert!(too_many_failures(&call("edit"), Some("EDIT_REFUSED"), &mut f, None).is_none());
        let n = too_many_failures(&call("edit"), Some("EDIT_REFUSED"), &mut f, None);
        assert!(n.is_some(), "second same-code failure is named");
        assert!(n.unwrap().contains("edit"));
    }

    #[test]
    fn a_different_code_starts_a_fresh_count() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_REFUSED"), &mut f, None);
        // A genuinely different error is a new situation, not a loop.
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_RENUMBERED"), &mut f, None).is_none(),
            "different code must not count against the old one"
        );
    }

    #[test]
    fn a_success_clears_the_streak_except_for_edits() {
        let mut f = Failures::new();
        too_many_failures(&call("bash"), Some("BASH_TIMEOUT"), &mut f, None);
        note_success(&call("bash"), &mut f);
        assert!(f.is_empty(), "a bash success breaks the bash streak");

        // Landing one edit does not mean the next will land, so its streak
        // stays until the model changes approach.
        too_many_failures(&call("edit"), Some("EDIT_REFUSED"), &mut f, None);
        note_success(&call("edit"), &mut f);
        assert!(
            f.contains_key(&("edit".into(), "EDIT_REFUSED".into())),
            "an edit success keeps the edit streak"
        );
    }

    #[test]
    fn a_failure_after_naming_starts_a_fresh_count() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_REFUSED"), &mut f, None);
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_REFUSED"), &mut f, None).is_some(),
            "two in a row are named"
        );
        // The naming reset the count: one isolated mistake after the loop was
        // broken is a new situation, not the Nth repeat of the old one.
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_REFUSED"), &mut f, None).is_none(),
            "a single failure after naming must not be called a repeat"
        );
    }
}
