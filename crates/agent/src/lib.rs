use std::collections::HashMap;
use std::sync::Arc;

use brain::catalog::ModelSpec;
use brain::message::{Message, ToolCall, ToolResult};
use brain::request::{Effort, Request};
use brain::stream::{Accumulator, StreamEvent};
use brain::transport::Transport;
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tools::{Concurrency, Ctx, Registry, ToolError, ToolOutput};

use crate::log::Log;

pub mod approval;
pub mod compact;
pub mod event;
pub mod log;
pub mod summarize;

pub use approval::{Approver, Ceiling, Decision};
pub use compact::Policy;
pub use event::{Event, Totals};

pub const DEFAULT_SYSTEM: &str = include_str!("../prompts/system.md");

/// Headroom for framing the estimate does not model. Compacting slightly early
/// costs a little quality; compacting late costs the whole turn.
const SAFETY_MARGIN: usize = 2_000;

/// How hard to squeeze after the provider says the request did not fit. Our
/// estimate was wrong by an unknown amount, so the correction is blunt.
const SQUEEZE: f64 = 0.6;

/// Attempts to shrink one turn before giving up on it.
const MAX_SQUEEZE: usize = 3;

/// Retry schedule for a request the provider could not serve right now.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: usize,
    pub base: std::time::Duration,
    pub max: std::time::Duration,
    /// No data for this long means the stream is wedged. Generous, because a
    /// reasoning model can legitimately think for minutes before its first
    /// token.
    pub idle: std::time::Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 4,
            base: std::time::Duration::from_millis(800),
            max: std::time::Duration::from_secs(30),
            idle: std::time::Duration::from_secs(300),
        }
    }
}

impl Retry {
    /// Exponential, capped, with jitter so concurrent agents do not retry in
    /// lockstep against a provider that is already struggling.
    fn delay(&self, attempt: usize) -> std::time::Duration {
        let grown = self.base.saturating_mul(1u32 << attempt.min(10));
        let capped = grown.min(self.max);
        // Nanos from the clock are a good enough jitter source for a backoff,
        // and cheaper than taking on a rng dependency.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0) as u64;
        let jitter = capped.as_millis() as u64 / 4;
        capped + std::time::Duration::from_millis(if jitter == 0 { 0 } else { nanos % jitter })
    }
}

/// What a run leaves behind.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub totals: Totals,
    /// Present when the run ended through the `yield` tool.
    pub yielded: Option<serde_json::Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Brain(#[from] brain::BrainError),

    #[error("cancelled")]
    Cancelled,

    #[error("stopped at the {0}-turn limit")]
    TurnLimit(usize),

    #[error("the run ended without calling `yield`, so it produced no result")]
    NoResult,
}

/// The transcript. Held by the caller so a run that ends in an error still
/// leaves behind everything it produced.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub log: Log,
}

impl Session {
    pub fn with_prompt(prompt: impl Into<String>) -> Self {
        let mut log = Log::new();
        log.push(Message::user(prompt));
        Self { log }
    }

    /// Continue an existing transcript with a new prompt.
    pub fn resumed(mut log: Log, prompt: impl Into<String>) -> Self {
        log.resume(prompt);
        Self { log }
    }

    /// What the model sees this turn.
    pub fn context(&self) -> Vec<Message> {
        self.log.context()
    }
}

pub struct Agent {
    pub transport: Arc<dyn Transport>,
    pub spec: ModelSpec,
    pub registry: Registry,
    pub approver: Arc<dyn Approver>,
    pub system: String,
    pub effort: Effort,
    pub max_turns: usize,
    /// None leaves the transcript alone and lets the provider refuse it.
    pub compaction: Option<Policy>,
    /// Ask the model to summarize the history compaction is about to drop.
    /// False drops it outright, which is faster and loses more.
    pub summarize: bool,
    pub retry: Retry,
    /// When set, the run must end by calling this tool, and its argument is the
    /// result. Set by adding a `yield` tool to the registry.
    pub finish_tool: Option<String>,
}

/// What a streamed call resolves to before anything runs. Deciding first keeps
/// the result list aligned with the call list even when nothing executes.
enum Action {
    Reject(String),
    Run(Arc<dyn tools::Tool>),
}

impl Agent {
    pub fn new(transport: Arc<dyn Transport>, spec: ModelSpec) -> Self {
        Self {
            transport,
            spec,
            registry: Registry::builtin(),
            approver: Arc::new(Ceiling(tools::Tier::Exec)),
            system: DEFAULT_SYSTEM.to_string(),
            effort: Effort::Off,
            max_turns: 50,
            compaction: Some(Policy::default()),
            summarize: true,
            retry: Retry::default(),
            finish_tool: None,
        }
    }

    pub async fn run(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<Outcome, AgentError> {
        let mut totals = Totals::default();
        let mut nudged = false;

        // A resumed session brings its plan back; the tool sees it as the list
        // it left behind rather than an empty one.
        if let Ok(mut held) = ctx.todos.lock() {
            *held = session.log.todos().to_vec();
        }

        // Our token estimate is a bound, not a measurement. When the provider
        // says otherwise, this is what carries the correction forward.
        let mut scale = 1.0f64;
        // A window the provider named, which outranks whatever the catalog says.
        let mut hard: Option<usize> = None;

        for turn in 1..=self.max_turns {
            let _ = tx.send(Event::TurnStart { turn });
            let mut squeezes = 0usize;

            let done = loop {
                let budget = ((hard.unwrap_or_else(|| self.budget()) as f64) * scale) as usize;
                self.maybe_compact(session, budget, squeezes > 0, &mut totals, tx)
                    .await;

                let req = Request {
                    system: Some(self.system.clone()),
                    messages: session.context(),
                    tools: self.registry.defs(),
                    max_output_tokens: None,
                    temperature: None,
                    effort: self.effort,
                    tool_choice: Default::default(),
                };

                match self.stream_turn(&req, ctx, tx).await {
                    Ok(done) => break done,
                    Err(AgentError::Brain(e))
                        if brain::classify(&e) == brain::Fault::Overflow
                            && squeezes < MAX_SQUEEZE =>
                    {
                        squeezes += 1;
                        // The refusal usually names the real window. Reading it
                        // beats guessing when the estimate was wrong by an
                        // unknown amount.
                        match brain::fault::overflow_limit(&e) {
                            Some(limit) => {
                                hard = Some(self.budget_within(limit));
                                let _ = tx.send(Event::Warning(format!(
                                    "the provider reports a {limit}-token window; refitting to it"
                                )));
                            }
                            None => {
                                scale *= SQUEEZE;
                                let _ = tx.send(Event::Warning(format!(
                                    "the request did not fit and named no limit; \
                                     retrying at {}% of the estimated budget",
                                    (scale * 100.0).round()
                                )));
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            };

            let cost = self.spec.cost(&done.usage);
            totals.add(&done.usage, cost);
            let _ = tx.send(Event::TurnEnd {
                usage: done.usage,
                cost,
            });

            // Two providers accept an oversized request instead of refusing it:
            // one silently, one by truncating and then having no room to answer.
            // Both look like success and neither can be caught before the fact.
            let window = self.spec.context_window as usize;
            let silently_truncated = done.usage.input as usize > window
                || (done.stop == brain::StopReason::MaxTokens && done.usage.output == 0);
            if silently_truncated && scale > SQUEEZE.powi(MAX_SQUEEZE as i32) {
                scale *= SQUEEZE;
                let _ = tx.send(Event::Warning(format!(
                    "the provider took {} input tokens against a {window}-token window and \
                     answered from a truncated prompt; tightening the budget",
                    done.usage.input
                )));
            }

            let calls: Vec<ToolCall> = done.message.tool_calls().cloned().collect();
            session.log.push(done.message);

            if calls.is_empty() {
                // A run that owes a structured result gets one reminder; the
                // model usually just forgot the last step.
                if self.finish_tool.is_some() && self.yielded(ctx).is_none() {
                    if nudged {
                        return Err(AgentError::NoResult);
                    }
                    nudged = true;
                    session.log.push(Message::user(
                        "The result has not been delivered yet. Call `yield` with it now.",
                    ));
                    continue;
                }
                let _ = tx.send(Event::Done {
                    turns: turn,
                    usage: totals.usage,
                    cost: totals.cost,
                });
                return Ok(Outcome {
                    totals,
                    yielded: self.yielded(ctx),
                });
            }

            let bad: HashMap<_, _> = done
                .invalid
                .iter()
                .map(|i| (i.call.clone(), i.error.clone()))
                .collect();
            let results = self.run_calls(&calls, &bad, ctx, tx).await?;
            session.log.push(Message::tool_results(results));
            self.record_todos(session, ctx);

            if let Some(value) = self.yielded(ctx) {
                let _ = tx.send(Event::Done {
                    turns: turn,
                    usage: totals.usage,
                    cost: totals.cost,
                });
                return Ok(Outcome {
                    totals,
                    yielded: Some(value),
                });
            }
        }

        Err(AgentError::TurnLimit(self.max_turns))
    }

    fn yielded(&self, ctx: &Ctx) -> Option<serde_json::Value> {
        ctx.yielded.lock().ok()?.clone()
    }

    /// Fold a plan the todo tool wrote into the session, so it survives
    /// compaction and comes back on resume.
    fn record_todos(&self, session: &mut Session, ctx: &Ctx) {
        let Ok(mut held) = ctx.todos.lock() else {
            return;
        };
        if *held == session.log.todos() {
            return;
        }
        session.log.set_todos(held.clone());
        // The log normalizes; writing it back keeps the next comparison from
        // seeing a difference that is only the normalization.
        *held = session.log.todos().to_vec();
    }

    /// Shrink the transcript to `budget` if it is over, recording what went.
    async fn maybe_compact(
        &self,
        session: &mut Session,
        budget: usize,
        urgent: bool,
        totals: &mut Totals,
        tx: &UnboundedSender<Event>,
    ) {
        let Some(policy) = &self.compaction else {
            return;
        };
        if brain::estimate::tokens(&session.context()) <= budget {
            return;
        }
        // Holding the working tail back is a preference; fitting at all is not.
        // Once the provider has refused the request, the tail yields.
        let policy = if urgent {
            compact::Policy { protect_tail: 0 }
        } else {
            *policy
        };
        let (mut record, mut report) = compact::plan(&session.log, budget, &policy);
        if !record.dropped.is_empty() && self.summarize {
            let cost = self.write_summary(&session.log, &mut record).await;
            report.summarized = record.summary.is_some();
            totals.add(&cost, self.spec.cost(&cost));
        }
        // A pass that reclaimed nothing is not news; reporting it every turn
        // buries the ones that did.
        if report.touched() {
            session.log.record(record);
            let _ = tx.send(Event::Compacted(report));
        }
    }

    /// Summarize what is about to be dropped, folding in any summary already in
    /// force and retiring it.
    ///
    /// A failure here is not fatal: the entries still go, unsummarized. Losing
    /// the summary costs context; failing the turn costs the whole run.
    async fn write_summary(&self, log: &Log, record: &mut log::Compaction) -> brain::stream::Usage {
        let history = summarize::render(&log.summaries(), &log.messages_for(&record.dropped));
        match summarize::run(&*self.transport, &self.spec, history).await {
            Ok((text, usage)) => {
                record.summary = Some(text);
                // The new summary covers what the old one did, so the entry
                // carrying the old one leaves the view.
                record.dropped.extend(log.summary_entries());
                usage
            }
            Err(e) => {
                tracing::warn!("summarizing dropped history failed: {e}");
                brain::stream::Usage::default()
            }
        }
    }

    /// What the transcript may occupy. The reply, the system prompt and the
    /// tool schemas all share the window with it, so each is subtracted before
    /// the transcript gets to claim what is left.
    pub fn budget(&self) -> usize {
        self.budget_within(self.spec.context_window as usize)
    }

    /// The same accounting against a window the provider named instead of the
    /// one the catalog claims.
    fn budget_within(&self, window: usize) -> usize {
        // A spec may declare an output cap larger than the window it is being
        // used against — an overridden window, a proxy, a stale entry. Reserving
        // it verbatim would leave the transcript nothing at all.
        let reply = (self.spec.max_output_tokens as usize).min(window / 4);
        let fixed = brain::estimate::text(&self.system)
            + brain::estimate::tool_defs(&self.registry.defs())
            + reply
            + SAFETY_MARGIN;
        // Even an unworkable configuration leaves a floor: stripping the
        // transcript to nothing helps no one.
        window.saturating_sub(fixed).max(window / 4)
    }

    /// Run one request, retrying while the provider says it is a passing problem.
    async fn stream_turn(
        &self,
        req: &Request,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<brain::stream::Completion, AgentError> {
        let mut attempt = 0usize;
        loop {
            let err = match self.attempt(req, ctx, tx).await {
                Ok(done) => return Ok(done),
                Err(AgentError::Brain(e)) => e,
                Err(other) => return Err(other),
            };

            // A spent quota arrives as a 429 like any throttle; retrying that
            // one only costs money.
            if attempt >= self.retry.attempts || brain::classify(&err) != brain::Fault::Transient {
                return Err(AgentError::Brain(err));
            }

            attempt += 1;
            let delay = self.retry.delay(attempt);
            let _ = tx.send(Event::Retrying {
                attempt,
                delay_ms: delay.as_millis() as u64,
                reason: err.to_string(),
            });
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = ctx.cancel.cancelled() => return Err(AgentError::Cancelled),
            }
        }
    }

    /// One attempt. Deltas reach the renderer as they arrive, so a retry shows
    /// as a false start — which the Retrying event is there to explain.
    async fn attempt(
        &self,
        req: &Request,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<brain::stream::Completion, AgentError> {
        // A fresh accumulator per attempt: half a stream must not bleed into
        // the message the retry produces.
        let mut acc = Accumulator::new(self.transport.name(), &self.spec.wire_id);
        let idle = self.retry.idle;

        let mut stream = tokio::select! {
            r = tokio::time::timeout(idle, self.transport.stream(&self.spec, req)) => match r {
                Ok(r) => r?,
                Err(_) => return Err(wedged(idle)),
            },
            _ = ctx.cancel.cancelled() => return Err(AgentError::Cancelled),
        };

        loop {
            let next = tokio::select! {
                n = tokio::time::timeout(idle, stream.next()) => match n {
                    Ok(n) => n,
                    // A provider that stops sending mid-stream would otherwise
                    // hold the turn open until the user gives up.
                    Err(_) => return Err(wedged(idle)),
                },
                _ = ctx.cancel.cancelled() => return Err(AgentError::Cancelled),
            };
            let Some(ev) = next else { break };
            let ev = ev?;
            match &ev {
                StreamEvent::TextDelta { delta, .. } => {
                    let _ = tx.send(Event::TextDelta(delta.clone()));
                }
                StreamEvent::ReasoningDelta { delta, .. } => {
                    let _ = tx.send(Event::ReasoningDelta(delta.clone()));
                }
                _ => {}
            }
            acc.push(ev);
        }

        Ok(acc.finish())
    }

    /// Every call gets exactly one result, in call order: an unanswered
    /// `tool_use` makes the next request invalid on both wires.
    async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<brain::message::ToolCallId, String>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<Vec<ToolResult>, AgentError> {
        let actions: Vec<Action> = calls
            .iter()
            .map(|c| {
                if let Some(err) = bad.get(&c.id) {
                    return Action::Reject(format!(
                        "arguments were not valid JSON ({err}); send the whole object again"
                    ));
                }
                let Some(tool) = self.registry.get(&c.name) else {
                    return Action::Reject(format!(
                        "no tool named `{}`; available: {}",
                        c.name,
                        self.registry.names().join(", ")
                    ));
                };
                match self.approver.approve(&c.name, tool.tier(), &c.args) {
                    Decision::Allow => Action::Run(tool),
                    Decision::Deny(why) => Action::Reject(why),
                }
            })
            .collect();

        for (call, action) in calls.iter().zip(&actions) {
            match action {
                Action::Reject(why) => {
                    let _ = tx.send(Event::ToolDenied {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        reason: why.clone(),
                    });
                }
                Action::Run(_) => {
                    let _ = tx.send(Event::ToolStart {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        args: call.args.clone(),
                    });
                }
            }
        }

        // Building one future per call and awaiting them positionally is what
        // keeps results aligned; completion order never reaches the transcript.
        let exclusive = actions
            .iter()
            .any(|a| matches!(a, Action::Run(t) if t.concurrency() == Concurrency::Exclusive));

        let mut outputs: Vec<Option<Result<ToolOutput, ToolError>>> =
            Vec::with_capacity(calls.len());
        if exclusive {
            for (call, action) in calls.iter().zip(&actions) {
                outputs.push(match action {
                    Action::Reject(_) => None,
                    Action::Run(t) => Some(t.execute(call.args.clone(), ctx).await),
                });
            }
        } else {
            let futures: Vec<_> = calls
                .iter()
                .zip(&actions)
                .map(|(call, action)| async move {
                    match action {
                        Action::Reject(_) => None,
                        Action::Run(t) => Some(t.execute(call.args.clone(), ctx).await),
                    }
                })
                .collect();
            outputs = futures::future::join_all(futures).await;
        }

        let mut results = Vec::with_capacity(calls.len());
        for ((call, action), output) in calls.iter().zip(&actions).zip(outputs) {
            let result = match (action, output) {
                (Action::Reject(why), _) => ToolResult::error(call.id.clone(), &call.name, why),
                (_, Some(Err(ToolError::Cancelled))) => return Err(AgentError::Cancelled),
                (_, Some(Err(e))) => {
                    let body = e.to_string();
                    let _ = tx.send(Event::ToolEnd {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        is_error: true,
                        preview: body.clone(),
                    });
                    ToolResult::error(call.id.clone(), &call.name, body)
                }
                (_, Some(Ok(out))) => {
                    let _ = tx.send(Event::ToolEnd {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        is_error: false,
                        preview: out.preview(),
                    });
                    ToolResult {
                        call: call.id.clone(),
                        provider: call.provider.clone(),
                        name: call.name.clone(),
                        content: out.content,
                        is_error: false,
                        useless: out.useless,
                    }
                }
                (_, None) => unreachable!("only rejected calls produce no output"),
            };
            results.push(result);
        }

        Ok(results)
    }
}

fn wedged(idle: std::time::Duration) -> AgentError {
    AgentError::Brain(brain::BrainError::Stream(format!(
        "the stream sent nothing for {}s",
        idle.as_secs()
    )))
}

/// Cancels on the first Ctrl-C so a runaway turn stops without killing the
/// process mid-write.
pub fn cancel_on_interrupt() -> CancellationToken {
    let token = CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            child.cancel();
        }
    });
    token
}
