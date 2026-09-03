use std::collections::HashMap;
use std::sync::Arc;

use brain::model::ModelSpec;
use brain::message::{Message, ToolCall, ToolResult};
use brain::request::{Effort, Request};
use brain::stream::{Accumulator, InvalidToolArgs, StreamEvent};
use brain::transport::Transport;
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tools::{Concurrency, Ctx, Registry, ToolError, ToolOutput};
use tracing::Instrument as _;

use crate::session::Session;

pub mod approval;
pub mod compact;
pub mod event;
pub mod session;
pub mod summarize;

pub use approval::{Approver, Ceiling, Decision};
pub use compact::Policy;
use event::say;
pub use event::{Event, Totals};

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

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Brain(#[from] brain::BrainError),

    #[error("cancelled")]
    Cancelled,
}


#[derive(Clone)]
pub struct Agent {
    pub transport: Arc<dyn Transport>,
    pub spec: ModelSpec,
    pub registry: Registry,
    pub approver: Arc<dyn Approver>,
    pub system: String,
    pub effort: Effort,
    /// None leaves the transcript alone and lets the provider refuse it.
    pub compaction: Option<Policy>,
    /// Ask the model to summarize the history compaction is about to drop.
    /// False drops it outright, which is faster and loses more.
    pub summarize: bool,
    /// Who writes that summary, when it is not the model doing the work. The
    /// job is large input, small output and little judgement, so it need not be
    /// the expensive one. Its own transport all the same: the spec that priced
    /// a turn has to be the one that ran it, or a cheap summary is billed at
    /// the working model's rate.
    pub summarizer: Option<(Arc<dyn Transport>, ModelSpec)>,
    pub retry: Retry,
}

// Per-tool failure streaks across one run, so a loop can be named. Keyed by
// tool and the stable code its error carries: a patch that keeps coming back
// "would not parse" is one loop whatever the prose says, while a genuinely
// different error starts a new count.
type Failures = HashMap<(String, String), usize>;

// What a streamed call resolves to before anything runs. Deciding first keeps
// the result list aligned with the call list even when nothing executes.
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
            compaction: Some(Policy::default()),
            summarize: true,
            summarizer: None,
            retry: Retry::default(),
        }
    }

    /// Point the same run at a different host, and the budget at that host's.
    pub fn retarget(&mut self, transport: Arc<dyn Transport>, spec: ModelSpec) {
        self.transport = transport;
        self.spec = spec;
    }

    pub async fn run(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<Totals, AgentError> {
        let mut totals = Totals::default();
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
            say(tx, Event::TurnStart { turn });
            // Entered around each await rather than held across them: a guard
            // spanning an await point labels whatever else the runtime polls.
            let span = tracing::info_span!(target: "pi::loop", "turn", turn);
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
                let (messages, shrunk) = self
                    .maybe_compact(session, budget, squeezes > 0, &mut totals, tx)
                    .instrument(span.clone())
                    .await;
                sent = messages;
                if shrunk {
                    compactions += 1;
                }
                used = brain::estimate::tokens(&sent, &self.spec);
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
                    effort = ?self.effort,
                    "sending"
                );
                let req = Request {
                    system: Some(self.system.clone()),
                    messages: sent.clone(),
                    notes: Vec::new(),
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
                    Err(e) => return Err(e),
                }
            };

            let cost = self.spec.cost(&done.usage);
            totals.add(&done.usage, cost);
            say(
                tx,
                Event::TurnEnd {
                    usage: done.usage,
                    cost,
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
            let Message::Assistant { content, .. } = done.message else {
                unreachable!("the accumulator only ever builds an assistant message")
            };
            session.push_assistant(content);

            if calls.is_empty() {
                say(
                    tx,
                    Event::Done {
                        turns: turn,
                        usage: totals.usage,
                        cost: totals.cost,
                        // Re-measured rather than reused: `used` is what went
                        // out, and the reply landed in the session since.
                        ctx: (
                            brain::estimate::tokens(&session.context(), &self.spec),
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
                .run_calls(&calls, &bad, ctx, tx, &mut failures)
                .instrument(span.clone())
                .await?;
            session.push_previewed(results);
        }

        unreachable!("an unlimited run can only leave by returning inside the loop")
    }

    /// Shrink the transcript to `budget` if it is over, recording what went,
    /// and hand back what to send.
    ///
    /// The context comes back rather than being rebuilt by the caller: this has
    /// to build one to measure, and when nothing changed that is exactly the
    /// one to send. Building it twice a turn walked every entry and cloned
    /// every block for an answer already in hand.
    async fn maybe_compact(
        &self,
        session: &mut Session,
        budget: usize,
        urgent: bool,
        totals: &mut Totals,
        tx: &UnboundedSender<Event>,
    ) -> (Vec<Message>, bool) {
        let measured = session.context();
        let Some(policy) = &self.compaction else {
            return (measured, false);
        };
        if brain::estimate::tokens(&measured, &self.spec) <= budget {
            return (measured, false);
        }
        // Holding the working tail back is a preference; fitting at all is not.
        // Once the provider has refused the request, the tail yields.
        let policy = compact::Policy {
            protect_tail: if urgent { 0 } else { self.tail_within(budget) },
            ..*policy
        };
        let (mut record, mut report) = compact::plan(session, &self.spec, budget, &policy);
        if !record.dropped.is_empty() && self.summarize {
            let (used, priced) = self
                .write_summary(session, &mut record, None)
                .instrument(tracing::info_span!(target: "pi::compact", "summarize"))
                .await;
            report.summarized = record.summary.is_some();
            totals.add(&used, priced);
        }
        // A pass that reclaimed nothing is not news; reporting it every turn
        // buries the ones that did.
        if !report.touched() {
            return (measured, false);
        }
        session.record(record);
        say(tx, Event::Compacted(report));
        // It changed, so the measurement above is stale.
        (session.context(), true)
    }

    /// What a manual compaction leaves alone, when there is one.
    pub fn kept_tokens(&self) -> Option<usize> {
        self.compaction.is_some().then(|| self.tail_within(self.budget()))
    }

    /// The working tail to hold back, against a transcript budget of `budget`.
    ///
    /// A flat 16k is a seventh of a 114k budget and more than a 9k one holds,
    /// and a tail the size of the budget leaves the drop tier nothing to take.
    fn tail_within(&self, budget: usize) -> usize {
        self.compaction.map_or(0, |p| p.protect_tail.min(budget / 4))
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
        let base = self.compaction?;
        let tail = self.tail_within(self.budget());
        let policy = compact::Policy { protect_tail: tail, ..base };
        let (mut record, mut report) = compact::plan(session, &self.spec, tail, &policy);
        let mut spent = Totals::default();
        if !record.dropped.is_empty() && self.summarize {
            let (used, priced) = self
                .write_summary(session, &mut record, focus)
                .instrument(tracing::info_span!(target: "pi::compact", "summarize"))
                .await;
            report.summarized = record.summary.is_some();
            spent.add(&used, priced);
        }
        if !report.touched() {
            return None;
        }
        session.record(record);
        Some((report, spent))
    }

    /// Summarize what is about to be dropped, folding in any summary already in
    /// force and retiring it.
    ///
    /// A failure here is not fatal: the entries still go, unsummarized. Losing
    /// the summary costs context; failing the turn costs the whole run.
    ///
    /// Returns the usage *and what it cost*, because only here is it known
    /// which spec priced it. Handing back a bare usage let both callers pick a
    /// spec themselves, and both picked the main model's — so a cheaper
    /// summarizer would have been billed at the expensive model's rates, twice
    /// over and without a word.
    async fn write_summary(
        &self,
        session: &Session,
        record: &mut session::Compaction,
        focus: Option<&str>,
    ) -> (brain::stream::Usage, f64) {
        let (transport, spec) = match &self.summarizer {
            Some((t, s)) => (&**t, s),
            None => (&*self.transport, &self.spec),
        };
        let history =
            summarize::render(&session.summaries(), &session.entries_for(&record.dropped));
        match summarize::run(transport, spec, history, focus).await {
            Ok((text, usage)) => {
                record.summary = Some(text);
                // The new summary covers what the old one did, so the entry
                // carrying the old one leaves the view.
                record.dropped.extend(session.summary_entries());
                (usage, spec.cost(&usage))
            }
            Err(e) => {
                tracing::warn!(target: "pi::compact", error = %e, "summarizing dropped history failed");
                (brain::stream::Usage::default(), 0.0)
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
    /// one the spec claims.
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
        let mut acc = Accumulator::new(self.spec.model.clone());
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

        // What the host owed and did not send. Said once per session by the
        // reporter itself, and said here rather than only in the journal: a
        // turn that quietly came back smaller looks exactly like an ordinary
        // one, which is the whole reason it needs saying.
        for gap in self.transport.gaps() {
            say(tx, Event::Warning(gap));
        }

        Ok(acc.finish())
    }

    /// Every call gets exactly one result, in call order: an unanswered
    /// `tool_use` makes the next request invalid on both wires.
    async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<String, InvalidToolArgs>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        failures: &mut Failures,
    ) -> Result<Vec<(ToolResult, Option<String>)>, AgentError> {
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
            // What the screen showed, when a tool sketched more than its stored
            // content holds. The rebuild has no other way back to it.
            let mut sketched = None;
            let result = match (action, output) {
                (Action::Reject(why), _) => failed(call, why.clone(), None, failures),
                (_, Some(Err(ToolError::Cancelled))) => return Err(AgentError::Cancelled),
                (_, Some(Err(e))) => {
                    let mut body = e.to_string();
                    if let Some(code) = e.code() {
                        // A stable code lets the model branch on what happened
                        // instead of parsing the prose; the prose still leads.
                        body = format!("Error: {body} [code: {code}]");
                    }
                    say(
                        tx,
                        Event::ToolEnd {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            is_error: true,
                            preview: body.clone(),
                        },
                    );
                    failed(call, body, e.category(), failures)
                }
                (_, Some(Ok(out))) => {
                    sketched = out.preview.clone();
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
            results.push((result, sketched));
        }

        Ok(results)
    }
}

// A success resets the failure streak for this tool — the loop-breaker only
// names an unbroken run of failures — except for edit, whose every success is
// a different file: landing one edit does not mean the next will land, and a
// malformed-patch loop must keep being counted until the model actually
// changes approach.
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
) -> ToolResult {
    if let Some(notice) = too_many_failures(call, code, failures) {
        body.push_str(&notice);
    }
    ToolResult::error(call.id.clone(), &call.name, body)
}

// Name a tool whose failures are piling up. The count is per tool and per
// stable error code, so the wording of the refusal — which a loop keeps
// changing — never matters: a patch that keeps coming back refused the same
// way is a loop, whatever the prose says, while a genuinely different error
// starts a new count. Two failures is already the whole story; the second
// tells the model nothing the first did not, so there is no leeway the way
// there is for a re-read. Naming resets the count, so a mistake made long
// after the loop was broken is not called the Nth repeat of it.
fn too_many_failures(
    call: &ToolCall,
    code: Option<&'static str>,
    failures: &mut Failures,
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
    Some(format!(
        "\n[the same `{}` call has now failed the same way {seen} times. \
         Sending it again will not change the answer — change the call, \
         or reach the goal another way.]",
        call.name
    ))
}

fn wedged(idle: std::time::Duration) -> AgentError {
    AgentError::Brain(brain::BrainError::Stream(format!(
        "the stream sent nothing for {}s",
        idle.as_secs()
    )))
}

// Conventional exit code for a process killed by SIGINT.
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

#[cfg(test)]
mod tests {
    use super::*;
    use brain::message::ToolCall;

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
        assert!(too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f).is_none());
        let n = too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f);
        assert!(n.is_some(), "second same-code failure is named");
        assert!(n.unwrap().contains("edit"));
    }

    #[test]
    fn a_different_code_starts_a_fresh_count() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f);
        // A genuinely different error is a new situation, not a loop.
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_RENUMBERED"), &mut f).is_none(),
            "different code must not count against the old one"
        );
    }

    #[test]
    fn an_edit_success_does_not_clear_its_failure_streak() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f);
        note_success(&call("edit"), &mut f);
        // Landing one edit does not mean the next will land, so its streak
        // stays until the model changes approach.
        assert!(f.contains_key(&("edit".into(), "EDIT_UNBALANCED".into())));
    }

    #[test]
    fn a_success_clears_the_streak_for_any_other_tool() {
        let mut f = Failures::new();
        too_many_failures(&call("bash"), Some("BASH_TIMEOUT"), &mut f);
        note_success(&call("bash"), &mut f);
        assert!(f.is_empty(), "a bash success breaks the bash streak");
    }

    #[test]
    fn a_failure_after_naming_starts_a_fresh_count() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f);
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f).is_some(),
            "two in a row are named"
        );
        // The naming reset the count: one isolated mistake after the loop was
        // broken is a new situation, not the Nth repeat of the old one.
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f).is_none(),
            "a single failure after naming must not be called a repeat"
        );
    }

    #[test]
    fn short_invalid_args_are_shown_whole() {
        let raw = r#"{"path": "#;
        assert_eq!(
            invalid_args_snippet(raw, "EOF while parsing an object at line 1 column 9"),
            raw
        );
    }

    #[test]
    fn long_invalid_args_center_on_the_failing_column() {
        let raw = format!(r#"{{"path":"{}"}}"#, "a".repeat(600));
        // Column at the very end: the window must reach the tail, hiding the
        // head where the parse already succeeded.
        let tail = invalid_args_snippet(&raw, "control character found in string at line 1 column 611");
        assert!(tail.starts_with('…'), "{tail}");
        assert!(tail.ends_with('}'), "{tail}");
        assert!(tail.chars().count() <= MAX_INVALID_ARGS_SHOWN + 1, "{tail}");
        // Column near the start: the window must keep the head and hide the
        // tail instead.
        let head = invalid_args_snippet(&raw, "expected value at line 1 column 2");
        assert!(head.starts_with('{'), "{head}");
        assert!(head.ends_with('…'), "{head}");
    }

    #[test]
    fn long_invalid_args_without_a_column_fall_back_to_the_tail() {
        let raw = format!(r#"{{"path":"{}"}}"#, "b".repeat(600));
        let snippet = invalid_args_snippet(&raw, "not valid JSON");
        assert!(snippet.ends_with('}'), "{snippet}");
        assert!(snippet.starts_with('…'), "{snippet}");
    }
}
