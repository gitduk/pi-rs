use std::collections::HashMap;
use std::sync::Arc;

use llm::message::{AssistantContent, Message, ToolCall};
use llm::meter::{Meter, Metered};
use llm::model::ModelSpec;
use llm::request::{Effort, Request};
use llm::stream::Usage;
use llm::transport::Transport;
use tokio::sync::mpsc::UnboundedSender;
use tool::{Ctx, Registry};
use tracing::Instrument as _;

use crate::session::{Measured, Session};

pub mod approval;
mod calls;
pub mod compaction;
pub mod event;
pub mod hooks;
pub mod retry;
pub mod seams;
mod turn;
pub use transcript as session;

pub use approval::Ceiling;
pub use compaction::Summarizing;
pub use compaction::ladder::{Policy, Report};
use event::Voice;
pub use event::{Event, Totals};
pub use hooks::{GatesOnly, Hooks, NoHooks};
pub use retry::Retry;
pub use seams::{Approver, Archive, Compactor, Decision, Fitted, Untouched, Working};

// Headroom for what the last count could not see: the turn's new entries are
// converted at the last request's ratio, not their own.
const SAFETY_MARGIN: usize = 2_000;

// How hard to squeeze after the provider says the request did not fit. Our
// conversion was wrong by an unknown amount, so the correction is blunt.
const SQUEEZE: f64 = 0.6;

// Attempts to shrink one turn before giving up on it.
const MAX_SQUEEZE: usize = 3;

/// How long a run has to wind down after its stop token is tripped before it
/// is dropped where it stands.
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] llm::LlmError),

    #[error("cancelled")]
    Cancelled,

    #[error("still running {} after cancel", llm::figures::elapsed(STOP_GRACE))]
    Unstopped,
}

/// Fold how the run that just ended went into the next prompt: a run that
/// did not answer its prompt records why.
pub trait NoteOutcome {
    fn note_outcome(&mut self, outcome: &Result<Usage, AgentError>);
}

impl NoteOutcome for Session {
    fn note_outcome(&mut self, outcome: &Result<Usage, AgentError>) {
        match outcome {
            Ok(_) => {}
            Err(AgentError::Cancelled) => self.note_user_stop(),
            Err(e) => self.note_failure(e.to_string()),
        }
    }
}

/// The wire a run talks on, and the model on the other end. Swapped whole when
/// the model changes, which moves nothing else about a run.
#[derive(Clone)]
pub struct Model {
    // Reached through `Agent::wire`, so no call can go out uncounted.
    pub(crate) transport: Arc<dyn Transport>,
    pub spec: ModelSpec,
}

/// What a run is allowed to do and what it is told. Rebuilt whole by whoever
/// sets the run up; a child run derives its own from its caller's.
#[derive(Clone)]
pub struct Briefing {
    pub registry: Registry,
    pub system: String,
    /// The system prompt a summary of dropped history is written under.
    pub summarize: String,
    pub effort: Effort,
    pub approver: Arc<dyn Approver>,
    /// What runs around each call the approver let through.
    pub hooks: Arc<dyn Hooks>,
}

#[derive(Clone)]
pub struct Agent {
    pub model: Arc<Model>,
    pub brief: Arc<Briefing>,
    /// What shrinks the transcript when it outgrows the window. The identity
    /// until something is installed — see `Compactor`.
    pub compactor: Arc<dyn Compactor>,
    /// What every call this agent and the children it starts make is charged
    /// to. A clone shares it; whoever owns a new one gives it a fresh meter.
    pub meter: Arc<Meter>,
}

impl Agent {
    /// An agent with no tools and no prompt: whoever assembles it installs
    /// both on the brief, as the identity compactor waits for a real one.
    pub fn new(transport: Arc<dyn Transport>, spec: ModelSpec) -> Self {
        Self {
            model: Arc::new(Model { transport, spec }),
            brief: Arc::new(Briefing {
                registry: Registry::new(),
                system: String::new(),
                summarize: compaction::SUMMARIZE.to_string(),
                effort: Effort::Off,
                approver: Arc::new(Ceiling(tool::Tier::Exec)),
                hooks: Arc::new(NoHooks),
            }),
            compactor: Arc::new(seams::Untouched),
            meter: Arc::default(),
        }
    }

    /// The wire every call goes out on, counted on this agent's meter.
    pub fn wire(&self) -> Metered<'_> {
        Metered::new(&*self.model.transport, &self.meter)
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

    /// Run the session's prompt until the model stops calling tools.
    ///
    /// `retry` is read where the run starts, not kept on the agent, so a
    /// reload that changed it reaches the next run without a refresh.
    ///
    /// A run that ends unanswered closes the calls it left open before it
    /// returns, so a transcript at rest never holds a dangling `tool_use`.
    pub async fn run(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        retry: &Retry,
    ) -> Result<Usage, AgentError> {
        let voice = Voice::new(tx.clone(), self.brief.hooks.clone());
        // Tool sources are live: what they list now, not when the brief was applied.
        tracing::info!(
            target: "pi::loop",
            system_bytes = self.brief.system.len(),
            tools_bytes = llm::request::defs_bytes(&self.brief.registry.defs()),
            "run start"
        );
        if let Some(note) = self.brief.hooks.ahead(ctx).await {
            let id = session.push_note(note);
            voice.say(Event::Committed {
                entries: session.entries_for(&[id]).into_iter().cloned().collect(),
            });
        }
        let out = self.turns(session, ctx, &voice, retry).await;
        match &out {
            Ok(_) => {}
            Err(AgentError::Cancelled) => session.close_open(session::STOPPED_CALL),
            Err(_) => session.close_open(session::DIED_CALL),
        }
        out
    }

    async fn turns(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        voice: &Voice,
        retry: &Retry,
    ) -> Result<Usage, AgentError> {
        let mut totals = Usage::default();

        // The provider's count converts our bound; when a request still does
        // not fit, this is what carries the correction forward.
        let mut scale = 1.0f64;
        // A window the provider named, which outranks whatever the spec says.
        let mut named_window: Option<usize> = None;
        // Across the whole run, not the turn: a status line that reset this
        // every turn would report "not compacted" for a run that just was.
        let mut compactions = 0usize;
        // The provider's count of the run's latest request, against its window.
        let mut held = None;

        for turn in 1.. {
            voice.say(Event::TurnStart { turn });
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
            // The attempt that answered says how long its reply took to stream.
            let mut generating = None;
            // Kept past the retry loop: the provider's count of the request
            // that ran is paired with that request's bound and window.
            let mut window;
            let mut bound;

            let done = loop {
                window = named_window.unwrap_or(self.model.spec.context_window as usize);
                let measured = self.count_of(session);
                let budget = ((self.budget_within(window, measured) as f64) * scale) as usize;
                // Uncounted, the request goes as it is: only a refusal says it
                // does not fit, and the squeeze that follows fits it.
                let fit_to = if measured.is_none() && squeezes == 0 {
                    usize::MAX
                } else {
                    budget
                };
                let fitted = self
                    .compactor
                    .compact(session, self.working(), fit_to, squeezes > 0, voice)
                    .instrument(span.clone())
                    .await;
                totals.add(&fitted.spent);
                sent = as_sent(fitted.context, &self.model.spec);
                if fitted.changed {
                    compactions += 1;
                    held = None;
                }
                bound = self.overhead() + llm::estimate::tokens(&sent, &self.model.spec);
                tracing::debug!(
                    target: "pi::loop",
                    parent: &span,
                    messages = sent.len(),
                    bound,
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
                    .stream_turn(&req, ctx, voice, retry, &mut generating)
                    .instrument(span.clone())
                    .await
                {
                    Ok(done) => break done,
                    Err((AgentError::Llm(e), _))
                        if llm::classify(&e) == llm::Fault::Overflow && squeezes < MAX_SQUEEZE =>
                    {
                        squeezes += 1;
                        let named = llm::fault::overflow_limit(&e).map(|limit| {
                            (limit, self.budget_within(limit, self.count_of(session)))
                        });
                        match named {
                            Some((limit, refit)) if refit < budget => {
                                // Earlier squeezes guessed against the spec's
                                // window; the provider's replaces that baseline.
                                if named_window.is_none() {
                                    scale = 1.0;
                                }
                                named_window = Some(limit);
                                voice.say(Event::Refit(format!(
                                    "the provider reports a {limit}-token window; refitting to it"
                                )));
                            }
                            // A window the request was already fitted to means
                            // the estimate was off: refitting would resend it.
                            _ => {
                                scale *= SQUEEZE;
                                voice.say(Event::Refit(format!(
                                    "the request did not fit and named no limit below the \
                                     budget; retrying at {}% of the estimated budget",
                                    (scale * 100.0).round()
                                )));
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
            voice.say(Event::TurnEnd {
                usage: done.usage,
                text: done.message.text(),
                generating,
            });
            let prompt = done.usage.prompt() as usize;
            // A provider that counted nothing leaves the last count standing.
            if prompt > 0 {
                session.set_measured(Measured {
                    model: self.model.spec.model.clone(),
                    tokens: prompt,
                    bound,
                });
                held = Some((prompt, window));
                voice.say(Event::Context {
                    used: prompt,
                    window,
                });
            }

            // Two providers accept an oversized request instead of refusing:
            // one silently, one by truncating — both look like success.
            let window = self.model.spec.context_window as usize;
            let silently_truncated = prompt > window
                || (done.stop == llm::StopReason::MaxTokens && done.usage.output == 0);
            if silently_truncated && scale > SQUEEZE.powi(MAX_SQUEEZE as i32) {
                scale *= SQUEEZE;
                voice.say(Event::Refit(format!(
                    "the provider took {prompt} input tokens against a {window}-token window \
                     and answered from a truncated prompt; tightening the budget"
                )));
            }

            // Sizes beside the stop: a reply that ends after two words reads
            // like any other turn unless the record says how much it said.
            let (mut text, mut reasoning) = (0usize, 0usize);
            if let Message::Assistant { content, .. } = &done.message {
                for c in content {
                    match c {
                        AssistantContent::Text(t) => text += t.text.chars().count(),
                        AssistantContent::Reasoning(r) => reasoning += r.text().chars().count(),
                        AssistantContent::ToolCall(_) => {}
                    }
                }
            }
            tracing::info!(
                target: "pi::loop",
                parent: &span,
                stop = ?done.stop,
                bound,
                text,
                reasoning,
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
                voice.say(Event::Done {
                    turns: turn,
                    usage: totals,
                    ctx: held,
                    compactions,
                });
                return Ok(totals);
            }

            let bad: HashMap<_, _> = done
                .invalid
                .iter()
                .map(|i| (i.call.clone(), i.clone()))
                .collect();
            let (results, stopped) = self
                .run_calls(&calls, &bad, ctx, voice, &mut totals)
                .instrument(span.clone())
                .await;
            let ids = session.push_previewed(results);
            // A state update, not a drawing instruction: whoever draws derives
            // the results' rows from these entries itself.
            voice.say(Event::Committed {
                entries: session.entries_for(&ids).into_iter().cloned().collect(),
            });
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
    pub async fn compact_now(&self, session: &mut Session, focus: Option<&str>) -> Option<Report> {
        let budget = self.budget_within(self.window(), self.count_of(session));
        let report = self
            .compactor
            .compact_now(session, self.working(), budget, focus)
            .await;
        // No voice to say it through, so journal it as the loop's would be.
        if let Some(r) = report {
            event::journal(&Event::Compacted(r));
        }
        report
    }

    /// The model and wire for whatever needs both — the compactor writes
    /// its summary with these when it has no writer of its own.
    fn working(&self) -> Working<'_> {
        Working {
            wire: self.wire(),
            spec: &self.model.spec,
            summarize: &self.brief.summarize,
        }
    }

    // What rides every request besides the transcript, in the bound's units:
    // the system prompt and the tool schemas.
    fn overhead(&self) -> usize {
        llm::estimate::text(&self.brief.system)
            + llm::estimate::tool_defs(&self.brief.registry.defs())
    }

    // The session's last count, if this model made it: another tokenizer's
    // ratio says nothing about this one's.
    fn count_of<'a>(&self, session: &'a Session) -> Option<&'a Measured> {
        session
            .measured()
            .filter(|m| m.model == self.model.spec.model)
    }

    // The window the spec claims, before any provider has named another.
    fn window(&self) -> usize {
        self.model.spec.context_window as usize
    }

    /// What the transcript may occupy before any request has been counted, in
    /// the bound's units, against the spec's window.
    pub fn budget(&self) -> usize {
        self.budget_within(self.window(), None)
    }

    // In the bound's units, converted at the provider's last count; uncounted,
    // the bound stands in for the count, which it errs above.
    fn budget_within(&self, window: usize, measured: Option<&Measured>) -> usize {
        let room = window.saturating_sub(self.reply_within(window) + SAFETY_MARGIN);
        let room = measured.map_or(room, |m| room * m.bound / m.tokens.max(1));
        // Even an unworkable configuration leaves a floor: stripping the
        // transcript to nothing helps no one.
        room.saturating_sub(self.overhead()).max(room / 4)
    }

    // The reply's share of `window`, reserved by the budget and sent as the
    // request's output cap. A spec's cap may exceed the window it runs against.
    fn reply_within(&self, window: usize) -> usize {
        (self.model.spec.max_output_tokens as usize).min(window / 4)
    }
}

// The context as the wire carries it: what a model without vision is sent
// in place of its images. Measured here, so every reported size agrees.
pub(crate) fn as_sent(mut context: Vec<Message>, spec: &ModelSpec) -> Vec<Message> {
    if !spec.vision {
        llm::message::blind(&mut context, &spec.model);
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Entry;

    #[test]
    fn only_a_failed_run_leaves_a_note_for_the_next_prompt() {
        for (why, outcome, noted) in [
            ("an answer", Ok(Usage::default()), false),
            ("a cancelled run", Err(AgentError::Cancelled), false),
            (
                "a failed run",
                Err(AgentError::Llm(llm::LlmError::Stream("died".into()))),
                true,
            ),
        ] {
            let mut s = Session::new();
            s.prompt("go");
            s.note_outcome(&outcome);
            s.send_prompt("and now this", None);
            let got = s.entries().iter().any(|e| matches!(e, Entry::Note { .. }));
            assert_eq!(got, noted, "{why}");
        }
    }
}
