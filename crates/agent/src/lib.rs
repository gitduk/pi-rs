use std::collections::HashMap;
use std::sync::Arc;

use brain::model::ModelSpec;
use brain::message::{Message, ToolCall, ToolResult, ToolResultContent};
use brain::request::{Effort, Request};
use brain::stream::{Accumulator, StreamEvent};
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

// Identical answers before the loop is named. Two is not enough: a re-read
// after compaction is legitimate, and the content really is the same.
const REPEAT_LIMIT: usize = 2;

// Identical failures before the same is said. Lower, because none of that
// leeway applies: nothing legitimate re-sends, byte for byte, a call that just
// came back refused. Waiting for a third spends a turn learning what the
// second already showed — which is how a model that cannot get a tool's format
// right spends a whole session getting it wrong the same way.
const FAILURE_LIMIT: usize = 1;

// Headroom for framing the estimate does not model. Compacting slightly early
// costs a little quality; compacting late costs the whole turn.
const SAFETY_MARGIN: usize = 2_000;

// How hard to squeeze after the provider says the request did not fit. Our
// estimate was wrong by an unknown amount, so the correction is blunt.
const SQUEEZE: f64 = 0.6;

// Attempts to shrink one turn before giving up on it.
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

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Brain(#[from] brain::BrainError),

    #[error("cancelled")]
    Cancelled,
}


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

// What each call last returned and how many times running, keyed by the call
// itself — its name and the arguments it was given.
type Echoes = HashMap<(String, String), (String, usize)>;

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
        // What each call last returned, so a loop can be named — naming it is
        // the only thing that stops one.
        let mut echoes = Echoes::new();

        // A resumed session brings its plan back; the tool sees it as the list
        // it left behind rather than an empty one.
        if let Ok(mut held) = ctx.todos.lock() {
            *held = session.todos().to_vec();
        }

        // Our token estimate is a bound, not a measurement. When the provider
        // says otherwise, this is what carries the correction forward.
        let mut scale = 1.0f64;
        // A window the provider named, which outranks whatever the spec says.
        let mut hard: Option<usize> = None;

        for turn in 1.. {
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
                sent = self
                    .maybe_compact(session, budget, squeezes > 0, &mut totals, tx)
                    .instrument(span.clone())
                    .await;
                tracing::debug!(
                    target: "pi::loop",
                    parent: &span,
                    messages = sent.len(),
                    estimated = brain::estimate::tokens(&sent, &self.spec),
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
                    },
                );
                return Ok(totals);
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
            session.push_previewed(results);
            self.record_todos(session, ctx);
        }

        unreachable!("an unlimited run can only leave by returning inside the loop")
    }

    /// Fold a plan the todo tool wrote into the session, so it survives a
    /// resume and `mark` has the list its numbers refer to.
    fn record_todos(&self, session: &mut Session, ctx: &Ctx) {
        let Ok(held) = ctx.todos.lock() else {
            return;
        };
        session.set_todos(held.clone());
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
    ) -> Vec<Message> {
        let measured = session.context();
        let Some(policy) = &self.compaction else {
            return measured;
        };
        if brain::estimate::tokens(&measured, &self.spec) <= budget {
            return measured;
        }
        // Holding the working tail back is a preference; fitting at all is not.
        // Once the provider has refused the request, the tail yields.
        let policy = if urgent {
            compact::Policy { protect_tail: 0, ..Policy::default() }
        } else {
            *policy
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
            return measured;
        }
        session.record(record);
        say(tx, Event::Compacted(report));
        // It changed, so the measurement above is stale.
        session.context()
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
        let (mut record, mut report) = compact::plan(session, &self.spec, policy.protect_tail, &policy);
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
        bad: &HashMap<String, String>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        echoes: &mut Echoes,
    ) -> Result<Vec<(ToolResult, Option<String>)>, AgentError> {
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
                (Action::Reject(why), _) => failed(call, why.clone(), echoes),
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
                    failed(call, body, echoes)
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
                    let body = out.flatten();
                    let mut content = out.content;
                    if let Some(notice) = repeat_notice(echoes, call, &body, Answer::Given) {
                        content.push(ToolResultContent::Text(brain::message::Text {
                            text: notice,
                        }));
                    }
                    ToolResult {
                        call: call.id.clone(),
                        name: call.name.clone(),
                        content,
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

// The scope a tool's own records land in, and what times it.
fn ran(call: &ToolCall) -> tracing::Span {
    tracing::info_span!(target: "pi::tool", "tool", name = %call.name, call = %call.id)
}

// Which way one call came back. The detection is the same either way; only the
// patience and the wording differ.
#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Given,
    Failed,
}

// A call that did not run, or ran and failed.
//
// The notice goes inside the error body rather than beside it: a failure has
// no content blocks to append one to, and the model reads the body.
fn failed(call: &ToolCall, mut body: String, echoes: &mut Echoes) -> ToolResult {
    if let Some(notice) = repeat_notice(echoes, call, &body, Answer::Failed) {
        body.push_str(&notice);
    }
    ToolResult::error(call.id.clone(), &call.name, body)
}

// Name a call that has come back identical too many times.
//
// A model that keeps making the same call and getting the same thing back is
// stuck, and nothing else stops it — there is no turn limit, so saying so is
// the only brake. Failures count the same as answers: a patch refused
// for the same reason three running is the commonest way a session dies, and
// counting only the answers left exactly that case unnamed.
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
