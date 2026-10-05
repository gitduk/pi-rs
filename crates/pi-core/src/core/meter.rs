//! What the events have said was spent, counted and read.
//!
//! Nothing here draws: the tally is a reading taken from a run's events, and
//! the snapshot is what a surface reads once it is time to say it.

use pi_store::icons;

use std::time::Duration;

use llm::model::Pricing;
use llm::stream::Usage;
use llm::totals::Totals;

/// Every value a status line can draw on, as far as it is known right now:
/// the run in flight's own figures, not the session's.
///
/// A zero count reads as a dash (the provider stated nothing); every other
/// zero drops its segment. Owned, not borrowed, so it outlives the lane.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub elapsed: Option<Duration>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// In dollars. Zero is an unpriced model rather than a free run.
    pub cost: f64,
    /// Turns begun. Zero is a run that has not started one.
    pub turns: usize,
    /// Used against usable, in tokens. The denominator is the budget, not the
    /// window, so 100% is where compaction fires rather than where it refuses.
    pub ctx: Option<(usize, usize)>,
    pub compactions: usize,
    pub queued: usize,
    pub model: String,
    /// The worktree the session is working in, or None in the repository's own
    /// checkout.
    pub worktree: Option<String>,
}

/// What the events have said was spent: one per lane and one per one-shot run,
/// holding the run in flight for the lines and the session for `/status`.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    // What earlier runs had spent when this one started, injected by the
    // surface; absent (a one-shot run) it is zero.
    base: Totals,
    // Turns of this run that have reported, and what they were priced at.
    settled: Totals,
    // The turn in flight; superseded (not added to) on `TurnEnd`, else the
    // input would count twice. Unpriced until it ends.
    turn: Usage,
    turns: usize,
    ctx: Option<(usize, usize)>,
    compactions: usize,
    // The rate this run is priced at, pinned at seed: costed at its own
    // model's rate regardless of what `/model` does meanwhile.
    pricing: Pricing,
}

impl Tally {
    /// Start a run's counts with the session's earlier runs already spent,
    /// and pin the rate this run is priced at. `/model` is answered at once,
    /// so a turn that began on one model and ends after a switch is still
    /// costed at what the model that ran it charges.
    pub fn seed(&mut self, base: Totals, pricing: Pricing) {
        *self = Self::default();
        self.base = base;
        self.pricing = pricing;
    }

    /// The rate this run is priced at.
    pub fn pricing(&self) -> Pricing {
        self.pricing
    }

    /// State the rate from outside a run: a surface that reads a receiver of
    /// its own knows when the model changed and the lane does not.
    pub fn set_pricing(&mut self, pricing: Pricing) {
        self.pricing = pricing;
    }

    /// Read one event for whatever number it carries, priced at the rate this
    /// run was seeded with — one source of that answer, so the line and the
    /// session total cannot disagree.
    pub fn on(&mut self, event: &agent::Event) {
        match event {
            agent::Event::TurnStart { turn } => self.turns = *turn,
            // A retry sends a second one for the same turn: the count it
            // carries replaces the abandoned attempt's rather than joining it.
            agent::Event::Usage(usage) => self.turn = *usage,
            agent::Event::TurnEnd { usage } => {
                self.settled.add(usage, self.pricing.cost(usage));
                self.turn = Usage::default();
            }
            agent::Event::Context { used, budget } => self.ctx = Some((*used, *budget)),
            agent::Event::Compacted(_) => self.compactions += 1,
            // Replaces the running count (not adds): a compaction summary
            // is a paid call with no event of its own, only caught here.
            agent::Event::Done {
                turns,
                usage,
                ctx,
                compactions,
            } => {
                self.settled = Totals {
                    usage: *usage,
                    cost: self.pricing.cost(usage),
                };
                self.turn = Usage::default();
                self.turns = *turns;
                self.ctx = Some(*ctx);
                self.compactions = *compactions;
            }
            _ => {}
        }
    }

    /// The one place a snapshot is built. What the events cannot say is asked
    /// for here: a one-shot run has no clock and no queue, and neither is a
    /// number the run reports.
    pub fn snapshot(
        &self,
        model: &str,
        worktree: Option<&str>,
        elapsed: Option<Duration>,
        queued: usize,
    ) -> Snapshot {
        let run = self.run_spend();
        Snapshot {
            elapsed,
            input: run.usage.input,
            output: run.usage.output,
            cache_read: run.usage.cache_read,
            cost: run.cost,
            turns: self.turns,
            ctx: self.ctx,
            compactions: self.compactions,
            queued,
            model: model.to_string(),
            worktree: worktree.map(str::to_string),
        }
    }

    /// The context used against the budget, as the last event said.
    pub fn ctx(&self) -> Option<(usize, usize)> {
        self.ctx
    }

    /// What this run has spent so far: the turns that have reported plus the
    /// one in flight. The lines read it, and the surface reads it again when a
    /// run ends without its own word — an interrupted turn — so the spend still
    /// lands in the totals.
    pub fn run_spend(&self) -> Totals {
        let mut t = self.settled;
        t.usage.add(&self.turn);
        t
    }

    /// What the session has spent, the run in flight included: every run of it
    /// this surface has watched. `/status` reads it; the lines say the run alone.
    pub fn session(&self) -> Totals {
        let mut t = self.base;
        t.merge(&self.run_spend());
        t
    }
}

// Takes the three pieces rather than a config entry: the running model may
// never have had one — a passed-through name has no entry to read.
pub(super) fn summary(format: &str, window: u32, p: &llm::model::Pricing) -> String {
    let mut parts = vec![format.to_string(), format!("{}k", window / 1000)];
    if p.input_per_mtok > 0.0 || p.output_per_mtok > 0.0 {
        parts.push(format!(
            "${:.2}/${:.2} per Mtok",
            p.input_per_mtok, p.output_per_mtok
        ));
    }
    parts.join(icons::PART_SEP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Default::default()
        }
    }

    // Round numbers, so the arithmetic an assertion checks is checkable by
    // eye: a token in costs 10 per million, one out 100.
    fn priced() -> Pricing {
        Pricing {
            input_per_mtok: 10.0,
            output_per_mtok: 100.0,
            ..Default::default()
        }
    }

    // A tally on the rate `priced()` states, which is what a run starts with.
    fn seeded() -> Tally {
        let mut t = Tally::default();
        t.seed(Totals::default(), priced());
        t
    }

    // The running count is the turns that have reported plus the one in
    // flight, and a turn's own report supersedes what it had said so far.
    #[test]
    fn a_tally_carries_the_finished_turns_and_the_one_in_flight() {
        let mut t = seeded();
        t.on(&agent::Event::TurnStart { turn: 1 });
        t.on(&agent::Event::Usage(usage(100, 5)));
        t.on(&agent::Event::Usage(usage(100, 20)));
        let mid = t.snapshot("m", None, None, 0);
        assert_eq!((mid.input, mid.output, mid.turns), (100, 20, 1));

        t.on(&agent::Event::TurnEnd {
            usage: usage(100, 30),
        });
        t.on(&agent::Event::TurnStart { turn: 2 });
        t.on(&agent::Event::Usage(usage(400, 7)));
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output, s.turns), (500, 37, 2));
        // 100 in at 10/mtok, 30 out at 100/mtok. The turn in flight is not
        // priced until it ends, so it contributes to neither figure.
        assert_eq!(s.cost, 0.004);
    }

    // A run's own total replaces the running one (same turns counted once):
    // an automatic compaction's summary is in it but in no event at all.
    #[test]
    fn a_finished_run_states_the_total_rather_than_adding_to_it() {
        let mut t = seeded();
        t.on(&agent::Event::TurnStart { turn: 1 });
        t.on(&agent::Event::TurnEnd {
            usage: usage(8_400, 390),
        });
        t.on(&agent::Event::Done {
            turns: 2,
            usage: usage(8_400, 390),
            ctx: (72_400, 114_000),
            compactions: 1,
        });
        let s = t.snapshot("m", None, None, 0);
        // The run's word replaces the running tally rather than joining it:
        // the same turns counted twice would double every number here.
        assert_eq!(s.turns, 2);
        assert_eq!((s.input, s.output), (8_400, 390));
        assert_eq!(s.cost, 0.123);
        assert_eq!(s.ctx, Some((72_400, 114_000)));
        assert_eq!(s.compactions, 1);
    }

    // Seeded with what the session spent before this run; that stays off
    // the lines — `session` is where the whole figure is asked for.
    #[test]
    fn a_seeded_tally_keeps_the_session_off_the_line() {
        let base = Totals {
            usage: Usage {
                input: 10_000,
                output: 4_000,
                cache_read: 300_000,
                ..Default::default()
            },
            cost: 0.02,
        };
        let mut t = seeded();
        t.seed(base, priced());
        t.on(&agent::Event::TurnStart { turn: 1 });
        t.on(&agent::Event::Usage(usage(100, 5)));
        let mid = t.snapshot("m", None, None, 0);
        assert_eq!((mid.input, mid.output, mid.cache_read), (100, 5, 0));
        assert_eq!(mid.cost, 0.0);
        let session = t.session();
        assert_eq!(
            (
                session.usage.input,
                session.usage.output,
                session.usage.cache_read
            ),
            (10_100, 4_005, 300_000)
        );
        assert_eq!(session.cost, 0.02);

        t.on(&agent::Event::Done {
            turns: 1,
            usage: usage(200, 30),
            ctx: (72_400, 114_000),
            compactions: 0,
        });
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output), (200, 30));
        assert_eq!(s.cost, 0.005);
        let session = t.session();
        assert_eq!((session.usage.input, session.usage.output), (10_200, 4_030));
        assert_eq!(session.cost, 0.025);
    }

    // A `!` command and a compaction begin no turn, and a host that reports
    // nothing has none to report: nothing may mint counts out of either.
    #[test]
    fn no_event_mints_counts() {
        let quiet = Tally::default().snapshot("m", None, None, 0);
        assert_eq!((quiet.input, quiet.output), (0, 0));

        let mut t = seeded();
        t.on(&agent::Event::TurnStart { turn: 1 });
        let started = t.snapshot("m", None, None, 0);
        assert_eq!((started.input, started.output), (0, 0));

        t.on(&agent::Event::Done {
            turns: 3,
            usage: Usage::default(),
            ctx: (0, 0),
            compactions: 0,
        });
        let s = t.snapshot("m", None, None, 0);
        assert_eq!(s.turns, 3);
        assert_eq!((s.input, s.output), (0, 0));
    }

    // Every field the events can fill is filled here; the rest (the
    // surface's own) has nowhere else to be patched in afterwards.
    #[test]
    fn a_snapshot_asks_for_what_no_event_states() {
        let s = Tally::default().snapshot("sonnet", Some("f1"), Some(Duration::from_secs(3)), 2);
        assert_eq!(s.model, "sonnet");
        assert_eq!(s.worktree.as_deref(), Some("f1"));
        assert_eq!(s.elapsed, Some(Duration::from_secs(3)));
        assert_eq!(s.queued, 2);
    }
}
