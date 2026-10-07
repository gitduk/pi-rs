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

pub mod approval;
pub mod compaction;
pub mod context;
pub mod event;
pub mod prompt;
pub mod retry;
pub mod seams;
pub mod session;

pub use approval::Ceiling;
pub use compaction::Summarizing;
pub use compaction::ladder::{Policy, Report};
use event::say;
pub use event::{Event, Totals};
pub use retry::Retry;
pub use seams::{Approver, Archive, Compactor, Decision, Fitted, Steer, Untouched, Working};

pub const DEFAULT_SYSTEM: &str = include_str!("../prompts/system.md");

// Headroom for framing the estimate does not model.
const SAFETY_MARGIN: usize = 2_000;

// How hard to squeeze after the provider says the request did not fit. Our
// estimate was wrong by an unknown amount, so the correction is blunt.
const SQUEEZE: f64 = 0.6;

// Attempts to shrink one turn before giving up on it.
const MAX_SQUEEZE: usize = 3;

// How much of a failed argument blob rides back to the model and log; past
// it, show a window around serde's column rather than the head.
const MAX_INVALID_ARGS_SHOWN: usize = 400;

/// How long a run has to wind down after its stop token is tripped before it
/// is dropped where it stands.
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] llm::LlmError),

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

/// What a run is allowed to do and what it is told. Rebuilt whole by a reload
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
    /// Swapped whole rather than written field by field, so a run in flight
    /// sees one brief or the other, never half of each.
    pub fn apply(&mut self, brief: Arc<Briefing>) {
        self.brief = brief;
    }

    /// A run nobody is talking to — what a subagent, a one-shot and a test
    /// all want, named for what it is rather than an empty mailbox each time.
    ///
    /// `retry` is read where the run starts, not kept on the agent, so a
    /// a reload that changed it reaches the next run without a refresh.
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

        // Our token estimate is a bound, not a measurement. When the provider
        // says otherwise, this is what carries the correction forward.
        let mut scale = 1.0f64;
        // A window the provider named, which outranks whatever the spec says.
        let mut named_window: Option<usize> = None;
        // Across the whole run, not the turn: a status line that reset this
        // every turn would report "not compacted" for a run that just was.
        let mut compactions = 0usize;

        for turn in 1.. {
            // What was said while the run worked. Read here and nowhere else:
            // a `tool_use` must be answered before anything else may speak.
            for said in steer.take() {
                session.prompt(said);
            }
            say(tx, Event::TurnStart { turn });
            // Entered around each await, not held across it — a guard spanning
            // an await labels whatever else the runtime polls meanwhile.
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
                let window = named_window.unwrap_or(self.model.spec.context_window as usize);
                budget = ((self.budget_within(window) as f64) * scale) as usize;
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
                    window,
                    effort = ?self.brief.effort,
                    "sending"
                );
                let req = Request {
                    system: Some(self.brief.system.clone()),
                    messages: sent,
                    tools: self.brief.registry.defs(),
                    // What the budget reserved: input + max_tokens past the
                    // window is a refusal on Anthropic.
                    max_output_tokens: Some(self.reply_within(window) as u32),
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
                    Err((AgentError::Llm(e), _))
                        if llm::classify(&e) == llm::Fault::Overflow && squeezes < MAX_SQUEEZE =>
                    {
                        squeezes += 1;
                        let named = llm::fault::overflow_limit(&e)
                            .map(|limit| (limit, self.budget_within(limit)));
                        match named {
                            Some((limit, refit)) if refit < budget => {
                                // Earlier squeezes guessed against the spec's
                                // window; the provider's replaces that baseline.
                                if named_window.is_none() {
                                    scale = 1.0;
                                }
                                named_window = Some(limit);
                                say(
                                    tx,
                                    Event::Warning(format!(
                                        "the provider reports a {limit}-token window; refitting to it"
                                    )),
                                );
                            }
                            // A window the request was already fitted to means
                            // the estimate was off: refitting would resend it.
                            _ => {
                                scale *= SQUEEZE;
                                say(
                                    tx,
                                    Event::Warning(format!(
                                        "the request did not fit and named no limit below the \
                                     budget; retrying at {}% of the estimated budget",
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

            // Two providers accept an oversized request instead of refusing:
            // one silently, one by truncating — both look like success.
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
                // The model stopped, but the user spoke while it was speaking:
                // posting `Done` here would leave that line for a second run.
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
            let (results, stopped) = self
                .run_calls(&calls, &bad, ctx, tx, &mut totals)
                .instrument(span.clone())
                .await;
            let ids = session.push_previewed(results);
            // A state update, not a drawing instruction: the renderer derives
            // the results' rows from these entries through `f_entry`.
            say(
                tx,
                Event::Committed {
                    entries: session.entries_for(&ids).into_iter().cloned().collect(),
                },
            );
            if stopped {
                return Err(AgentError::Cancelled);
            }
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
    /// Targets the same tail the automatic pass protects — "summarize
    /// everything but what I'm in the middle of" — and runs even when the
    /// transcript already fits, since the point is a phase ending, not a budget.
    pub async fn compact_now(
        &self,
        session: &mut Session,
        focus: Option<&str>,
    ) -> Option<(Report, Usage)> {
        self.compactor
            .compact_now(session, self.working(), self.budget(), focus)
            .await
    }

    /// The model and wire for whatever needs both — the compactor writes
    /// its summary with these when it has no writer of its own.
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
        let fixed = llm::estimate::text(&self.brief.system)
            + llm::estimate::tool_defs(&self.brief.registry.defs())
            + self.reply_within(window)
            + SAFETY_MARGIN;
        // Even an unworkable configuration leaves a floor: stripping the
        // transcript to nothing helps no one.
        window.saturating_sub(fixed).max(window / 4)
    }

    // The reply's share of `window`, reserved by the budget and sent as the
    // request's output cap. A spec's cap may exceed the window it runs against.
    fn reply_within(&self, window: usize) -> usize {
        (self.model.spec.max_output_tokens as usize).min(window / 4)
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
                AgentError::Llm(e) => e,
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
                return Err((AgentError::Llm(e), partial));
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
                Err(e) => return Err((AgentError::Llm(e), None)),
            },
            _ = ctx.cancel.cancelled() => return Err((AgentError::Cancelled, None)),
        };

        loop {
            let next = tokio::select! {
                n = leashed(idle, stream.next()) => match n {
                    Ok(n) => n,
                    // A provider that stops sending mid-stream would otherwise
                    // hold the turn open until the user gives up.
                    Err(e) => return Err((AgentError::Llm(e), None)),
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

        // What the host owed and did not send, said once per session by the
        // reporter. Said here too, not just the journal — else it looks ordinary.
        for gap in self.model.transport.gaps() {
            say(tx, Event::Warning(gap));
        }

        Ok(acc.finish())
    }

    // One result per call, in call order, except a cancelled one, left
    // unanswered for `send_prompt` to close; `spent` carries nested costs up.
    async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<String, InvalidToolArgs>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        spent: &mut Usage,
    ) -> (Vec<(ToolResult, Option<String>)>, bool) {
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

        // Awaited positionally, so results stay aligned with the calls.
        let futures: Vec<_> = calls
            .iter()
            .zip(&actions)
            .map(|(call, action)| {
                async move {
                    match action {
                        Action::Reject(why) => Landed::Answered(
                            ToolResult::error(call.id.clone(), &call.name, why.clone()),
                            None,
                        ),
                        Action::Run(t) => {
                            let output =
                                tool::output::gated(t.as_ref(), call.args.clone(), ctx).await;
                            landed(call, output, tx)
                        }
                    }
                }
                .instrument(ran(call))
            })
            .collect();
        // Neighbouring Shared calls join; an Exclusive one waits for those
        // ahead of it and holds back those behind.
        let mut outcomes: Vec<Landed> = Vec::with_capacity(calls.len());
        let mut shared = Vec::new();
        for (f, action) in futures.into_iter().zip(&actions) {
            if matches!(action, Action::Run(t) if t.concurrency() == Concurrency::Exclusive) {
                outcomes.extend(futures::future::join_all(std::mem::take(&mut shared)).await);
                outcomes.push(f.await);
            } else {
                shared.push(f);
            }
        }
        outcomes.extend(futures::future::join_all(shared).await);

        let mut results = Vec::with_capacity(calls.len());
        let mut stopped = false;
        for outcome in outcomes {
            match outcome {
                // Left unanswered: its siblings may have acted, so theirs are
                // kept, and the next prompt closes this one as stopped.
                Landed::Stopped => stopped = true,
                Landed::Answered(result, preview) => results.push((result, preview)),
                Landed::Ran(result, preview, used) => {
                    // A nested run's spend belongs to the run that called it:
                    // folded in here, it reaches `Event::Done` and the return.
                    spent.add(&used);
                    results.push((result, preview));
                }
            }
        }

        (results, stopped)
    }
}

// What one call came to. The preview is what the screen drew for it, sent
// along so a rebuild redraws those bytes instead of re-reading the content.
enum Landed {
    Stopped,
    Answered(ToolResult, Option<String>),
    Ran(ToolResult, Option<String>, Usage),
}

// A call's output as its result, its end said as it lands.
fn landed(
    call: &ToolCall,
    output: Result<ToolOutput, ToolError>,
    tx: &UnboundedSender<Event>,
) -> Landed {
    match output {
        Err(ToolError::Cancelled) => Landed::Stopped,
        Err(e) => {
            let mut body = e.to_string();
            if let Some(code) = e.code() {
                // A stable code lets the model branch on what happened
                // instead of parsing the prose; the prose still leads.
                body = format!("Error: {body} [code: {code}]");
            }
            // The pending live line draws from this same text, so a rebuild's
            // equality check sees one source, not a live/rebuilt mismatch.
            say(
                tx,
                Event::ToolEnd {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    is_error: true,
                    preview: body.clone(),
                },
            );
            Landed::Answered(
                ToolResult::error(call.id.clone(), &call.name, body.clone()),
                Some(body),
            )
        }
        Ok(out) => {
            say(
                tx,
                Event::ToolEnd {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    is_error: false,
                    preview: out.preview(),
                },
            );
            let result = ToolResult {
                call: call.id.clone(),
                name: call.name.clone(),
                content: out.content,
                is_error: false,
            };
            Landed::Ran(result, out.preview, out.spent)
        }
    }
}

fn ran(call: &ToolCall) -> tracing::Span {
    tracing::info_span!(target: "pi::tool", "tool", name = %call.name, call = %call.id)
}

// Shown whole when it fits, else windowed around serde's 1-indexed column
// (chars, not bytes — close enough); no column falls back to the tail.
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

fn wedged(idle: std::time::Duration) -> llm::LlmError {
    llm::LlmError::Stream(format!("the stream sent nothing for {}s", idle.as_secs()))
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
