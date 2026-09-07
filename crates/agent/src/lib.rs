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
mod oneshot;
pub mod remember;
pub mod session;
pub mod steer;
pub mod summarize;
pub mod task;

pub use approval::{Approver, Ceiling, Decision};
pub use compact::Policy;
pub use remember::{Kept, Shelf};
pub use steer::Steer;
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
    pub compaction: Policy,
    /// Who writes the summary, when it is not the model doing the work. The
    /// job is large input, small output and little judgement, so it need not be
    /// the expensive one. Its own transport all the same: the spec that priced
    /// a turn has to be the one that ran it, or a cheap summary is billed at
    /// the working model's rate.
    pub summarizer: Option<(Arc<dyn Transport>, ModelSpec)>,
    pub retry: Retry,
    /// Where facts that should outlive this session are kept. None for a
    /// subagent, a test or an embedder: a run nobody will return to has
    /// nothing to leave behind.
    /// Read into every turn's notes and written when compaction drops a span.
    ///
    /// A note rather than part of the system prompt, because it moves: the
    /// prompt is the cached prefix, and changing its tail re-bills every
    /// message behind it.
    pub shelf: Option<Arc<dyn remember::Shelf>>,
}

// Per-tool failure streaks across one run, so a loop can be named. Keyed by
// tool and the stable code its error carries: a patch that keeps coming back
// "would not parse" is one loop whatever the prose says, while a genuinely
// different error starts a new count.
type Failures = HashMap<(String, String), usize>;

/// The window, as the turn that ships it sees it. Shaped as a tag rather than
/// a sentence because a note is read in the user's voice: a reading is a fact
/// about the run, where a sentence would be someone talking.
fn context_note(used: usize, budget: usize, trimmed: bool) -> String {
    // Says only that the transcript shrank, never how: a summary that replaced
    // part of it is in the transcript to be read, where the taking is not. The
    // cheaper reclaims run before any drop and leave no summary behind.
    let trimmed = if trimmed { " trimmed=\"true\"" } else { "" };
    format!("<context used=\"{used}\" budget=\"{budget}\"{trimmed}/>")
}

/// Everything the turn says about itself, in the order it is read: what the
/// window is doing now, then what outlived the transcripts before it.
///
/// Read from the shelf each turn rather than held: compaction writes to it
/// mid-run, and a copy taken at startup would show the model a shelf without
/// the note it had just put there.
fn turn_notes(used: usize, budget: usize, trimmed: bool, shelf: Option<String>) -> Vec<String> {
    let mut out = vec![context_note(used, budget, trimmed)];
    out.extend(shelf.filter(|m| !m.trim().is_empty()));
    out
}

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
            compaction: Policy::default(),
            summarizer: None,
            shelf: None,
            retry: Retry::default(),
        }
    }

    /// Point the same run at a different host, and the budget at that host's.
    pub fn retarget(&mut self, transport: Arc<dyn Transport>, spec: ModelSpec) {
        self.transport = transport;
        self.spec = spec;
    }

    /// A run nobody is talking to. What a subagent, a `--print` and a test all
    /// want: named for what it is rather than passed an empty mailbox at every
    /// call site.
    pub async fn run(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<Totals, AgentError> {
        self.steered(session, ctx, tx, &Steer::default()).await
    }

    /// The same run, with somewhere for the user to speak into while it works.
    pub async fn steered(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        steer: &Steer,
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
            // Turn-scoped, not per-attempt: a squeeze retries the send, and the
            // attempt that lands still carries the shrunken transcript.
            let mut trimmed = false;
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
                    trimmed = true;
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
                    notes: turn_notes(
                        used,
                        budget,
                        trimmed,
                        self.shelf.as_ref().and_then(|s| s.read()),
                    ),
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
                .run_calls(&calls, &bad, ctx, tx, &mut failures, &mut totals)
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
        let policy = &self.compaction;
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
        if !record.dropped.is_empty() {
            let (used, priced) = self
                .retire_span(session, &mut record, None)
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

    /// What a manual compaction leaves alone.
    pub fn kept_tokens(&self) -> usize {
        self.tail_within(self.budget())
    }

    /// The working tail to hold back, against a transcript budget of `budget`.
    ///
    /// A flat 16k is a seventh of a 114k budget and more than a 9k one holds,
    /// and a tail the size of the budget leaves the drop tier nothing to take.
    fn tail_within(&self, budget: usize) -> usize {
        self.compaction.protect_tail.min(budget / 4)
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
        let base = self.compaction;
        let tail = self.tail_within(self.budget());
        let policy = compact::Policy { protect_tail: tail, ..base };
        let (mut record, mut report) = compact::plan(session, &self.spec, tail, &policy);
        let mut spent = Totals::default();
        if !record.dropped.is_empty() {
            let (used, priced) = self
                .retire_span(session, &mut record, focus)
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

    /// Ask what the span being dropped is worth, and to whom.
    ///
    /// Two judgements about one span, so one function and one round trip: a
    /// summary that carries this session's work forward, folding in any
    /// summary already in force and retiring it, and a few facts for the shelf
    /// that should outlive the session entirely.
    ///
    /// A failure in either is not fatal: the entries still go. Losing the
    /// summary costs context and losing a note costs a fact; failing the turn
    /// costs the whole run.
    ///
    /// Returns the usage *and what it cost*, because only here is it known
    /// which spec priced it. Handing back a bare usage let both callers pick a
    /// spec themselves, and both picked the main model's — so a cheaper
    /// summarizer would have been billed at the expensive model's rates, twice
    /// over and without a word.
    async fn retire_span(
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

        // Two judgements, one span, and neither is the other: the summary
        // carries this session's work forward, the shelf carries a few facts
        // past it. Together rather than in turn — one round trip, not two.
        let (summarized, kept) = futures::future::join(
            summarize::run(transport, spec, history.clone(), focus),
            self.fill_shelf(transport, spec, history, focus),
        )
        .await;

        let mut usage = kept;
        match summarized {
            Ok((text, used)) => {
                record.summary = Some(text);
                // The new summary covers what the old one did, so the entry
                // carrying the old one leaves the view.
                record.dropped.extend(session.summary_entries());
                usage.add(&used);
            }
            Err(e) => {
                tracing::warn!(target: "pi::compact", error = %e, "summarizing dropped history failed");
            }
        }
        let cost = spec.cost(&usage);
        (usage, cost)
    }

    /// Ask what should outlive the session and put it on the shelf, when there
    /// is one. Answers with what the asking cost, zero when nothing was asked.
    ///
    /// A failure is swallowed for the same reason the summary's is: losing a
    /// note costs a fact, failing the compaction costs the run.
    async fn fill_shelf(
        &self,
        transport: &dyn Transport,
        spec: &ModelSpec,
        history: String,
        focus: Option<&str>,
    ) -> brain::stream::Usage {
        let Some(shelf) = &self.shelf else {
            return brain::stream::Usage::default();
        };
        match remember::run(transport, spec, history, focus, shelf.read()).await {
            Ok((notes, usage)) => {
                if !notes.is_empty() {
                    tracing::info!(target: "pi::compact", kept = notes.len(), "shelved");
                    shelf.keep(notes);
                }
                usage
            }
            Err(e) => {
                tracing::warn!(target: "pi::compact", error = %e, "asking what to keep failed");
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
    ///
    /// `spent` is where a nested call's costs land — a subagent's whole run —
    /// so the run that called it reports them.
    async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<String, InvalidToolArgs>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        failures: &mut Failures,
        spent: &mut Totals,
    ) -> Result<Vec<(ToolResult, Option<String>)>, AgentError> {
        // Read once for the batch rather than per failure, and from `ctx`
        // rather than the machine: a session moves — `/new`, `/resume` — and
        // the context is what moves with it.
        let journal = ctx
            .session()
            .and_then(|id| tools::state::session_dir(ctx.workspace.root(), id))
            .map(|d| d.join("journal.jsonl"));
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
                (Action::Reject(why), _) => failed(call, why.clone(), None, failures, journal),
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
                    failed(call, body, e.category(), failures, journal)
                }
                (_, Some(Ok(out))) => {
                    // A nested run's spend belongs to the run that called it:
                    // folded in here, it reaches `Event::Done` and the return.
                    spent.merge(&out.spent);
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
    journal: Option<&std::path::Path>,
) -> ToolResult {
    if let Some(notice) = too_many_failures(call, code, failures, journal) {
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

    /// A reclaim is an event, not a running total: what the model has to act on
    /// is that its transcript just shrank, and that is true of one turn only. A
    /// count would be read as a standing fact on every later one.
    ///
    /// It claims a taking and never a summary. `plan` spends its cheaper
    /// measures before it drops anything, and a summary is written only for a
    /// drop — so on the common path there is nothing to promise.
    /// The window first, the shelf after: what the turn is doing now, then
    /// what outlived the transcripts before it. An empty shelf says nothing at
    /// all rather than an empty tag for the model to interpret.
    #[test]
    fn the_shelf_rides_the_turn_behind_the_window_reading() {
        let window = context_note(10, 100, false);
        assert_eq!(turn_notes(10, 100, false, None), vec![window.clone()]);
        assert_eq!(turn_notes(10, 100, false, Some("   ".into())), vec![window.clone()]);
        assert_eq!(
            turn_notes(10, 100, false, Some("<memory>\n2026-09-07 prefers xh\n</memory>".into())),
            vec![window, "<memory>\n2026-09-07 prefers xh\n</memory>".to_string()]
        );
    }

    #[test]
    fn a_reclaim_is_stated_on_the_turn_it_happened_and_claims_no_summary() {
        assert_eq!(
            context_note(118_000, 200_000, false),
            r#"<context used="118000" budget="200000"/>"#
        );
        assert_eq!(
            context_note(118_000, 200_000, true),
            r#"<context used="118000" budget="200000" trimmed="true"/>"#
        );
    }

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
        assert!(too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None).is_none());
        let n = too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None);
        assert!(n.is_some(), "second same-code failure is named");
        assert!(n.unwrap().contains("edit"));
    }

    /// The transcript says a call failed; only the journal says what the call
    /// actually carried. A loop is exactly when that difference starts to
    /// matter, so the notice that names the loop is where the journal is named.
    #[test]
    fn the_notice_points_at_the_journal_it_cannot_otherwise_reach() {
        let journal = std::path::Path::new("/fixture/sessions/-w/s1/journal.jsonl");
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, Some(journal));
        let notice =
            too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, Some(journal))
                .expect("the second same-code failure is named");

        assert!(notice.contains("/fixture/sessions/-w/s1/journal.jsonl"), "{notice}");
        // JSONL has no skeleton to fall back on, so a whole-file read is the
        // one way to spend the window that the pointer would have saved.
        assert!(notice.contains("grep"), "{notice}");

        // A machine with nowhere to keep a journal still gets the loop named.
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None);
        let bare = too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None)
            .expect("naming the loop does not depend on having a journal");
        assert!(bare.contains("failed the same way"), "{bare}");
        assert!(!bare.contains("grep"), "{bare}");
    }

    #[test]
    fn a_different_code_starts_a_fresh_count() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None);
        // A genuinely different error is a new situation, not a loop.
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_RENUMBERED"), &mut f, None).is_none(),
            "different code must not count against the old one"
        );
    }

    #[test]
    fn an_edit_success_does_not_clear_its_failure_streak() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None);
        note_success(&call("edit"), &mut f);
        // Landing one edit does not mean the next will land, so its streak
        // stays until the model changes approach.
        assert!(f.contains_key(&("edit".into(), "EDIT_UNBALANCED".into())));
    }

    #[test]
    fn a_success_clears_the_streak_for_any_other_tool() {
        let mut f = Failures::new();
        too_many_failures(&call("bash"), Some("BASH_TIMEOUT"), &mut f, None);
        note_success(&call("bash"), &mut f);
        assert!(f.is_empty(), "a bash success breaks the bash streak");
    }

    #[test]
    fn a_failure_after_naming_starts_a_fresh_count() {
        let mut f = Failures::new();
        too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None);
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None).is_some(),
            "two in a row are named"
        );
        // The naming reset the count: one isolated mistake after the loop was
        // broken is a new situation, not the Nth repeat of the old one.
        assert!(
            too_many_failures(&call("edit"), Some("EDIT_UNBALANCED"), &mut f, None).is_none(),
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
