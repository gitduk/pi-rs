//! Shrinking a transcript that outgrew the window. The compactor that ships:
//! `ladder` drops what the window cannot hold, and a model is asked what went.

pub mod ladder;
mod oneshot;
mod summary;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use llm::model::ModelSpec;
use llm::stream::Usage;
use llm::transport::Transport;
use tokio::sync::mpsc::UnboundedSender;
use tracing::Instrument as _;

use crate::event::{Event, say};
use crate::seams::{Compactor, Fitted, Working};
use crate::session;
use ladder::{Policy, Report};

/// The compactor that ships.
pub struct Summarizing {
    policy: Policy,
    /// Who writes the summary, when it is not the model doing the work. The job
    /// is large input, small output and little judgement, so it need not be the
    /// expensive one. Its own transport as well as its own spec: the spec that
    /// priced a turn has to be the one that ran it, or a cheap summary is
    /// billed at the working model's rate.
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
    ///
    /// A flat 16k is a seventh of a 114k budget and more than a 9k one holds,
    /// and a tail the size of the budget leaves the drop tier nothing to take.
    fn tail_within(&self, budget: usize) -> usize {
        self.policy.protect_tail.min(budget / 4)
    }

    /// One pass: plan against `budget`, have the span on its way out summarized,
    /// and record what went. `None` when there was nothing worth reclaiming.
    async fn pass(
        &self,
        session: &mut session::Session,
        run: Working<'_>,
        budget: usize,
        policy: &Policy,
        focus: Option<&str>,
    ) -> Option<(Report, Usage)> {
        let (mut record, mut report) = ladder::plan(session, run.spec, budget, policy);
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
        Some((report, spent))
    }

    /// Ask what the span being dropped is worth, and to whom.
    ///
    /// A summary that carries this session's work forward, folding in any
    /// summary already in force and retiring it.
    ///
    /// A failure is not fatal: the entries still go. Losing the summary costs
    /// context; failing the turn costs the whole run.
    ///
    /// Returns the tokens only. What they cost is the surface's arithmetic —
    /// see `run/meter.rs` — so a summarizer on a cheaper model is billed at the
    /// run's rate rather than its own. Worth saying out loud: it is why the
    /// number on the status line is an estimate, not an invoice.
    async fn retire_span(
        &self,
        session: &session::Session,
        run: Working<'_>,
        record: &mut session::Compaction,
        focus: Option<&str>,
    ) -> Usage {
        let (transport, spec) = match &self.writer {
            Some((t, s)) => (&**t, s),
            None => (run.transport, run.spec),
        };
        let history = summary::render(&session.summaries(), &session.entries_for(&record.dropped));

        match summary::run(transport, spec, history, focus, self.idle).await {
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
        tx: &UnboundedSender<Event>,
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
        match self.pass(session, run, budget, &policy, None).await {
            Some((report, spent)) => {
                say(tx, Event::Compacted(report));
                // It changed, so the measurement above is stale.
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
    ) -> Option<(Report, Usage)> {
        let tail = self.tail_within(budget);
        let policy = Policy {
            protect_tail: tail,
            ..self.policy
        };
        self.pass(session, run, tail, &policy, focus).await
    }

    fn kept_tokens(&self, budget: usize) -> usize {
        self.tail_within(budget)
    }
}
