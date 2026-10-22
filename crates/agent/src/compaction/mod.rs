//! Shrinking a transcript that outgrew the window. The compactor that ships:
//! `ladder` drops what the window cannot hold, and a model is asked what went.

pub mod ladder;
mod oneshot;
mod summary;

pub use oneshot::ask;
pub use summary::{PROMPT as SUMMARIZE, render};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use llm::model::ModelSpec;
use llm::stream::Usage;
use llm::transport::Transport;
use tracing::Instrument as _;

use crate::event::{Event, Voice};
use crate::seams::{Compactor, Fitted, Working};
use crate::session;
use ladder::{Policy, Report};

/// The compactor that ships.
pub struct Summarizing {
    policy: Policy,
    /// Who writes the summary, when it is not the model doing the work. The
    /// session's meter prices it at its own rate; the run's line, at the run's.
    writer: Option<(Arc<dyn Transport>, ModelSpec)>,
    /// How long a wedged stream is given before it is read as wedged, for the
    /// summary's own call.
    idle: Duration,
}

impl Summarizing {
    pub fn new(writer: Option<(Arc<dyn Transport>, ModelSpec)>, idle: Duration) -> Self {
        Self {
            policy: Policy::default(),
            writer,
            idle,
        }
    }

    /// The working tail to hold back, against a transcript budget of `budget`.
    /// Capped at a quarter of `budget`, so a small run still leaves room to drop.
    fn tail_within(&self, budget: usize) -> usize {
        self.policy.protect_tail.min(budget / 4)
    }

    /// One pass: plan against `target`, have the span on its way out summarized,
    /// and record what went. `None` when there was nothing worth reclaiming.
    /// `budget` is what the transcript may occupy, which `still_over` is against.
    async fn pass(
        &self,
        session: &mut session::Session,
        run: Working<'_>,
        target: usize,
        budget: usize,
        policy: &Policy,
        focus: Option<&str>,
    ) -> Option<(Report, Usage)> {
        let (mut record, mut report) = ladder::plan(session, run.spec, target, policy);
        let mut spent = Usage::default();
        if !record.dropped.is_empty() {
            let used = self
                .retire_span(session, run, &mut record, focus)
                .instrument(tracing::info_span!(target: "pi::compact", "summarize"))
                .await;
            report.summarized = record.summary.is_some();
            spent.add(&used);
        }
        // A pass that reclaimed nothing is not news; reporting it every turn
        // buries the ones that did.
        if !report.touched() {
            return None;
        }
        session.record(record);
        // Against what the plan fitted, which the images still count in.
        report.still_over = llm::estimate::tokens(&session.context(), run.spec) > budget;
        Some((report, spent))
    }

    /// Failure is not fatal: entries still go, only the summary is lost.
    /// Returns tokens, for the run's own figures; the wire charges the writer's
    /// rate to the meter as it goes.
    async fn retire_span(
        &self,
        session: &session::Session,
        run: Working<'_>,
        record: &mut session::Compaction,
        focus: Option<&str>,
    ) -> Usage {
        let (transport, spec) = match &self.writer {
            Some((t, s)) => (run.wire.over(&**t), s),
            None => (run.wire, run.spec),
        };
        let history = summary::render(&session.summaries(), &session.entries_for(&record.dropped));

        match summary::run(transport, spec, run.summarize, history, focus, self.idle).await {
            Ok((text, used)) => {
                record.summary = Some(text);
                // The new summary covers what the old one did, so the entry
                // carrying the old one leaves the view.
                record.dropped.extend(session.summary_entries());
                used
            }
            Err(e) => {
                tracing::warn!(target: "pi::compact", error = %e, "summarizing dropped history failed");
                Usage::default()
            }
        }
    }
}

#[async_trait]
impl Compactor for Summarizing {
    async fn compact(
        &self,
        session: &mut session::Session,
        run: Working<'_>,
        budget: usize,
        urgent: bool,
        voice: &Voice,
    ) -> Fitted {
        let measured = session.context();
        if llm::estimate::tokens(&measured, run.spec) <= budget {
            return Fitted {
                context: measured,
                changed: false,
                spent: Usage::default(),
            };
        }
        // Holding the working tail back is a preference; fitting at all is not.
        // Once the provider has refused the request, the tail yields.
        let policy = Policy {
            protect_tail: if urgent { 0 } else { self.tail_within(budget) },
            ..self.policy
        };
        // Settle at half: stopping just under the budget would compact again on
        // nearly every request after, each a summary call and a cold cache.
        match self
            .pass(session, run, budget / 2, budget, &policy, None)
            .await
        {
            Some((report, spent)) => {
                voice.say(Event::Compacted(report));
                Fitted {
                    context: session.context(),
                    changed: true,
                    spent,
                }
            }
            None => Fitted {
                context: measured,
                changed: false,
                spent: Usage::default(),
            },
        }
    }

    async fn compact_now(
        &self,
        session: &mut session::Session,
        run: Working<'_>,
        budget: usize,
        focus: Option<&str>,
    ) -> Option<Report> {
        let tail = self.tail_within(budget);
        let policy = Policy {
            protect_tail: tail,
            ..self.policy
        };
        let pass = self.pass(session, run, tail, budget, &policy, focus).await;
        pass.map(|(report, _)| report)
    }

    fn kept_tokens(&self, budget: usize) -> usize {
        self.tail_within(budget)
    }
}
