use std::collections::HashMap;
use std::sync::Arc;

use brain::catalog::ModelSpec;
use brain::message::{Message, ToolCall, ToolResult, ToolResultContent};
use brain::request::{Effort, Request};
use brain::stream::{Accumulator, StreamEvent};
use brain::transport::Transport;
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tools::{Concurrency, Ctx, Registry, ToolError, ToolOutput};
use tracing::Instrument as _;

use crate::log::Log;

pub mod approval;
pub mod compact;
pub mod event;
pub mod log;
pub mod summarize;

pub use approval::{Approver, Ceiling, Decision};
pub use compact::Policy;
use event::say;
pub use event::{Estimated, Event, Totals};

pub const DEFAULT_SYSTEM: &str = include_str!("../prompts/system.md");

/// Identical answers before the loop is named. Two is not enough: a re-read
/// after compaction is legitimate, and the content really is the same.
const REPEAT_LIMIT: usize = 2;

/// Identical failures before the same is said. Lower, because none of that
/// leeway applies: nothing legitimate re-sends, byte for byte, a call that just
/// came back refused. Waiting for a third spends a turn learning what the
/// second already showed — which is how a model that cannot get a tool's format
/// right rides the turn limit all the way down.
const FAILURE_LIMIT: usize = 1;

/// Headroom for framing the estimate does not model. Compacting slightly early
/// costs a little quality; compacting late costs the whole turn.
const SAFETY_MARGIN: usize = 2_000;

/// How hard to squeeze after the provider says the request did not fit. Our
/// estimate was wrong by an unknown amount, so the correction is blunt.
const SQUEEZE: f64 = 0.6;

/// Attempts to shrink one turn before giving up on it.
const MAX_SQUEEZE: usize = 3;

/// How far below our own count a reported input figure may sit before it stops
/// being a measurement. Deliberately generous: tokenizers disagree by tens of
/// percent, our own count is a bound rather than a reading, and a host may
/// exclude cached input under a name we do not parse. An order of magnitude is
/// none of those — a proxy reporting two hundred tokens for a transcript we
/// count in tens of thousands is not tokenizing differently, it is wrong.
const IMPLAUSIBLE: u64 = 10;

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

/// What each call last returned and how many times running, keyed by the call
/// itself — its name and the arguments it was given.
type Echoes = HashMap<(String, String), (String, usize)>;

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
        // What each call last returned, so a loop can be named rather than run
        // until the turn limit stops it.
        let mut echoes = Echoes::new();

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
            say(tx, Event::TurnStart { turn });
            // Entered around each await rather than held across them: a guard
            // spanning an await point labels whatever else the runtime polls.
            let span = tracing::info_span!(target: "pi::loop", "turn", turn);
            let mut squeezes = 0usize;
            // Kept past the retry loop: the fallback below prices what was
            // actually sent, which a squeeze or a compaction may have changed.
            let mut sent;

            let done = loop {
                let budget = ((hard.unwrap_or_else(|| self.budget()) as f64) * scale) as usize;
                self.maybe_compact(session, budget, squeezes > 0, &mut totals, tx)
                    .instrument(span.clone())
                    .await;

                sent = session.context();
                tracing::debug!(
                    target: "pi::loop",
                    parent: &span,
                    messages = sent.len(),
                    estimated = brain::estimate::tokens(&sent),
                    budget,
                    squeezes,
                    scale,
                    hard = hard.unwrap_or(0),
                    effort = ?self.effort,
                    "sending"
                );
                let req = Request {
                    system: Some(self.system.clone()),
                    messages: sent.clone(),
                    tools: self.registry.defs(),
                    max_output_tokens: None,
                    temperature: None,
                    effort: self.effort,
                    tool_choice: Default::default(),
                };

                match self
                    .stream_turn(&req, ctx, tx)
                    .instrument(span.clone())
                    .await
                {
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
                    Err(e) => return Err(e),
                }
            };

            let (usage, estimated) = self.fill_usage(done.usage, &sent, &done.message);
            let cost = self.spec.cost(&usage);
            totals.add_estimated(&usage, cost, estimated);
            say(
                tx,
                Event::TurnEnd {
                    usage,
                    cost,
                    estimated,
                },
            );

            // Two providers accept an oversized request instead of refusing it:
            // one silently, one by truncating and then having no room to answer.
            // Both look like success and neither can be caught before the fact.
            let window = self.spec.context_window as usize;
            let silently_truncated = done.usage.input as usize > window
                || (done.stop == brain::StopReason::MaxTokens && done.usage.output == 0);
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
                say(
                    tx,
                    Event::Done {
                        turns: turn,
                        usage: totals.usage,
                        cost: totals.cost,
                        estimated: totals.estimated,
                    },
                );
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
            let results = self
                .run_calls(&calls, &bad, ctx, tx, &mut echoes)
                .instrument(span.clone())
                .await?;
            let answered = session.log.push(Message::tool_results(results));
            if let Some(note) = turns_left(turn, self.max_turns) {
                // Amended onto the results rather than pushed after them: two
                // user messages running break the alternation both wires want.
                session.log.amend(answered, note);
            }
            self.record_todos(session, ctx);

            if let Some(value) = self.yielded(ctx) {
                say(
                    tx,
                    Event::Done {
                        turns: turn,
                        usage: totals.usage,
                        cost: totals.cost,
                        estimated: totals.estimated,
                    },
                );
                return Ok(Outcome {
                    totals,
                    yielded: Some(value),
                });
            }
        }

        tracing::error!(target: "pi::loop", turns = self.max_turns, "turn limit");
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
            let cost = self
                .write_summary(&session.log, &mut record, None)
                .instrument(tracing::info_span!(target: "pi::compact", "summarize"))
                .await;
            report.summarized = record.summary.is_some();
            totals.add(&cost, self.spec.cost(&cost));
        }
        // A pass that reclaimed nothing is not news; reporting it every turn
        // buries the ones that did.
        if report.touched() {
            session.log.record(record);
            say(tx, Event::Compacted(report));
        }
    }

    /// What a manual compaction leaves alone, when there is one.
    pub fn kept_tokens(&self) -> Option<usize> {
        self.compaction.map(|p| p.protect_tail)
    }

    /// Compact now, at the user's word rather than the window's.
    ///
    /// The target is the tail the agent is working from — the same number the
    /// automatic pass protects — so this means "summarize everything but what I
    /// am in the middle of". Unlike the automatic pass it runs even when the
    /// transcript already fits: the point is that the user knows a phase has
    /// ended, which no budget can tell.
    pub async fn compact_now(
        &self,
        session: &mut Session,
        focus: Option<&str>,
    ) -> Option<(compact::Report, Totals)> {
        let policy = self.compaction?;
        let (mut record, mut report) = compact::plan(&session.log, policy.protect_tail, &policy);
        let mut spent = Totals::default();
        if !record.dropped.is_empty() && self.summarize {
            let cost = self
                .write_summary(&session.log, &mut record, focus)
                .instrument(tracing::info_span!(target: "pi::compact", "summarize"))
                .await;
            report.summarized = record.summary.is_some();
            spent.add(&cost, self.spec.cost(&cost));
        }
        if !report.touched() {
            return None;
        }
        session.log.record(record);
        Some((report, spent))
    }

    /// Summarize what is about to be dropped, folding in any summary already in
    /// force and retiring it.
    ///
    /// A failure here is not fatal: the entries still go, unsummarized. Losing
    /// the summary costs context; failing the turn costs the whole run.
    async fn write_summary(
        &self,
        log: &Log,
        record: &mut log::Compaction,
        focus: Option<&str>,
    ) -> brain::stream::Usage {
        let history = summarize::render(&log.summaries(), &log.messages_for(&record.dropped));
        match summarize::run(&*self.transport, &self.spec, history, focus).await {
            Ok((text, usage)) => {
                record.summary = Some(text);
                // The new summary covers what the old one did, so the entry
                // carrying the old one leaves the view.
                record.dropped.extend(log.summary_entries());
                usage
            }
            Err(e) => {
                tracing::warn!(target: "pi::compact", error = %e, "summarizing dropped history failed");
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
    /// Fill in whatever the provider left at zero, and say that we did.
    ///
    /// A real request never costs zero input tokens, and a turn that produced a
    /// reply never costs zero output tokens; a zero in either place means the
    /// host reported nothing, not that nothing was sent. Filled per field
    /// rather than all-or-nothing, because a host that reports one and not the
    /// other is the ordinary case, not a broken one.
    ///
    /// What goes in is the same bound compaction runs on, so a turn reporting
    /// its own numbers reports the ones it is already steering by. It is the
    /// wrong bound for billing and the right one for noticing, which is why it
    /// travels marked.
    fn fill_usage(
        &self,
        reported: brain::stream::Usage,
        sent: &[Message],
        reply: &Message,
    ) -> (brain::stream::Usage, Estimated) {
        let mut usage = reported;
        let mut estimated = Estimated::default();
        let ours = (brain::estimate::tokens(sent)
            + brain::estimate::text(&self.system)
            + brain::estimate::tool_defs(&self.registry.defs())) as u64;
        // A host that reports nothing and a host that reports a number that
        // cannot be true are the same case: what it says is not what went.
        //
        // Any reported caching exempts it, writes as well as reads. A cached
        // prompt's `input` counts only the uncached remainder, so a small
        // figure beside a large one is exactly right — and the first turn of a
        // cached session is the sharpest version of that, where everything went
        // to `cache_write` and nothing has been read back yet. Checking reads
        // alone would call that turn a lie on every cached session there is.
        let uncached = usage.cache_read + usage.cache_write == 0;
        let cannot_be = uncached && usage.input < ours / IMPLAUSIBLE;
        if usage.input == 0 || cannot_be {
            if cannot_be {
                tracing::warn!(
                    target: "pi::wire",
                    reported = usage.input,
                    ours,
                    "the host's input count cannot be true; counting it ourselves"
                );
            }
            usage.input = ours;
            estimated.input = true;
        }
        if usage.output == 0 {
            usage.output = brain::estimate::message(reply) as u64;
            estimated.output = true;
        }
        (usage, estimated)
    }

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
                // The classification, not just the error: "why was this not
                // retried" is answerable from the fault and from nothing else.
                tracing::error!(
                    target: "pi::wire",
                    attempts = attempt,
                    fault = ?brain::classify(&err),
                    error = %err,
                    "giving up"
                );
                return Err(AgentError::Brain(err));
            }

            attempt += 1;
            let delay = self.retry.delay(attempt);
            say(
                tx,
                Event::Retrying {
                    attempt,
                    delay_ms: delay.as_millis() as u64,
                    reason: err.to_string(),
                },
            );
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
        echoes: &mut Echoes,
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
                    say(
                        tx,
                        Event::ToolDenied {
                            id: call.id.0.clone(),
                            name: call.name.clone(),
                            reason: why.clone(),
                        },
                    );
                }
                Action::Run(_) => {
                    say(
                        tx,
                        Event::ToolStart {
                            id: call.id.0.clone(),
                            name: call.name.clone(),
                            args: call.args.clone(),
                        },
                    );
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
                    Action::Run(t) => Some(
                        t.execute(call.args.clone(), ctx)
                            .instrument(ran(call))
                            .await,
                    ),
                });
            }
        } else {
            let futures: Vec<_> = calls
                .iter()
                .zip(&actions)
                .map(|(call, action)| {
                    async move {
                        match action {
                            Action::Reject(_) => None,
                            Action::Run(t) => Some(t.execute(call.args.clone(), ctx).await),
                        }
                    }
                    .instrument(ran(call))
                })
                .collect();
            outputs = futures::future::join_all(futures).await;
        }

        let mut results = Vec::with_capacity(calls.len());
        for ((call, action), output) in calls.iter().zip(&actions).zip(outputs) {
            let result = match (action, output) {
                (Action::Reject(why), _) => failed(call, why.clone(), echoes),
                (_, Some(Err(ToolError::Cancelled))) => return Err(AgentError::Cancelled),
                (_, Some(Err(e))) => {
                    let body = e.to_string();
                    say(
                        tx,
                        Event::ToolEnd {
                            id: call.id.0.clone(),
                            name: call.name.clone(),
                            is_error: true,
                            preview: body.clone(),
                        },
                    );
                    failed(call, body, echoes)
                }
                (_, Some(Ok(out))) => {
                    say(
                        tx,
                        Event::ToolEnd {
                            id: call.id.0.clone(),
                            name: call.name.clone(),
                            is_error: false,
                            preview: out.preview(),
                        },
                    );
                    let body = out.flatten();
                    let mut content = out.content;
                    if let Some(notice) = repeat_notice(echoes, call, &body, Answer::Given) {
                        content.push(ToolResultContent::Text(brain::message::Text {
                            text: notice,
                        }));
                    }
                    ToolResult {
                        call: call.id.clone(),
                        provider: call.provider.clone(),
                        name: call.name.clone(),
                        content,
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

/// How many turns are left, said at two points on the way down.
///
/// A run that stops at the limit stops mid-work, and the model never saw it
/// coming: the budget belongs to the caller and is stated nowhere the model can
/// read. One that knows batches what is left instead of spending a turn per
/// fix — which is how fifty turns go on eight rebuilds and no finished task.
fn turns_left(turn: usize, max: usize) -> Option<String> {
    match max.checked_sub(turn)? {
        10 => Some(format!(
            "[10 of {max} turns left. Batch what you can from here — \
             one fix per turn will not fit in what remains.]"
        )),
        3 => Some("[3 turns left. Finish and answer now; the run stops after that.]".into()),
        _ => None,
    }
}

/// The scope a tool's own records land in, and what times it.
fn ran(call: &ToolCall) -> tracing::Span {
    tracing::info_span!(target: "pi::tool", "tool", name = %call.name, call = %call.id.0)
}

/// Which way one call came back. The detection is the same either way; only the
/// patience and the wording differ.
#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Given,
    Failed,
}

/// A call that did not run, or ran and failed.
///
/// The notice goes inside the error body rather than beside it: a failure has
/// no content blocks to append one to, and the model reads the body.
fn failed(call: &ToolCall, mut body: String, echoes: &mut Echoes) -> ToolResult {
    if let Some(notice) = repeat_notice(echoes, call, &body, Answer::Failed) {
        body.push_str(&notice);
    }
    ToolResult::error(call.id.clone(), &call.name, body)
}

/// Name a call that has come back identical too many times.
///
/// A model that keeps making the same call and getting the same thing back is
/// stuck, and it will keep going until the turn limit stops it. Saying so is
/// what breaks the loop. Failures count the same as answers: a patch refused
/// for the same reason three running is the commonest way a session dies, and
/// counting only the answers left exactly that case unnamed.
fn repeat_notice(
    echoes: &mut Echoes,
    call: &ToolCall,
    body: &str,
    answer: Answer,
) -> Option<String> {
    // Arguments that would not parse arrive here as `{}` whatever they were, so
    // a malformed call is told apart by the refusal it drew rather than by what
    // it sent. That is the right grain: three calls that fail to parse the same
    // way are one loop, whichever bytes produced them.
    let key = (call.name.clone(), call.args.to_string());
    // Explicit rather than `entry().or_insert()`: the first call must not be
    // counted as a repeat of itself.
    let Some(slot) = echoes.get_mut(&key) else {
        echoes.insert(key, (body.to_string(), 0));
        return None;
    };
    if slot.0 != body {
        // A different answer: whatever the model was waiting for has happened.
        *slot = (body.to_string(), 0);
        return None;
    }
    slot.1 += 1;
    let seen = slot.1;
    let limit = match answer {
        Answer::Given => REPEAT_LIMIT,
        Answer::Failed => FAILURE_LIMIT,
    };
    (seen >= limit).then(|| {
        let times = seen + 1;
        tracing::warn!(
            target: "pi::tool",
            tool = %call.name,
            seen = times,
            failed = answer == Answer::Failed,
            "the same call keeps coming back the same way"
        );
        match answer {
            Answer::Given => format!(
                "\n[this is the same `{}` call with the same result, {times} times now. \
                 Nothing has changed — try something else.]",
                call.name
            ),
            Answer::Failed => format!(
                "\n[the same `{}` call has now failed the same way {times} times. \
                 Sending it again will not change the answer — change the call, \
                 or reach the goal another way.]",
                call.name
            ),
        }
    })
}

fn wedged(idle: std::time::Duration) -> AgentError {
    AgentError::Brain(brain::BrainError::Stream(format!(
        "the stream sent nothing for {}s",
        idle.as_secs()
    )))
}

/// Conventional exit code for a process killed by SIGINT.
const INTERRUPTED: i32 = 130;

/// First Ctrl-C cancels; a second one leaves.
///
/// `tokio::signal::ctrl_c` replaces SIGINT's default action for the whole
/// process and never restores it, so a handler that only fires once leaves no
/// way out at all — the second press has to do the killing itself.
pub fn cancel_on_interrupt() -> CancellationToken {
    let token = CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        child.cancel();
        eprintln!("\ninterrupting — press Ctrl-C again to quit");
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(INTERRUPTED);
        }
    });
    token
}
