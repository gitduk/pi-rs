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
    /// Percent of the prompt tokens this run sent that were read from cache,
    /// floored; `None` until a request of the run has reported its prompt.
    pub cached: Option<u64>,
    /// In dollars. Zero is an unpriced model rather than a free run.
    pub cost: f64,
    /// What the run's last request occupied against the model's window, as
    /// the provider counted it; `None` until one is counted.
    pub ctx: Option<(usize, usize)>,
    /// Output tokens a second, over the time the model spent streaming them;
    /// `None` until one turn of the run has streamed anything.
    pub speed: Option<f64>,
    pub compactions: usize,
    /// The run had to resend smaller than its estimate allowed.
    pub refit: bool,
    pub queued: usize,
    pub model: String,
    /// The worktree the session is working in, or None in the repository's own
    /// checkout.
    pub worktree: Option<String>,
}

/// What the events have said a run spent: one per lane and one per one-shot
/// run, for the lines that say the run alone. The session's figure is counted
/// at the wire instead, by `llm::meter`.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    // Turns of this run that have reported, and what they were priced at.
    settled: Totals,
    // The turn in flight; superseded (not added to) on `TurnEnd`, else the
    // input would count twice. Unpriced until it ends.
    turn: Usage,
    ctx: Option<(usize, usize)>,
    compactions: usize,
    // The run's warnings, kept whole for `/status`.
    compacted: Option<agent::Report>,
    refits: Vec<String>,
    gaps: Vec<String>,
    // Output tokens of the turns that streamed, and the time they streamed
    // for: the run's speed, not skewed by waits or tool calls between.
    generated: (u64, Duration),
    // The rate this run is priced at, pinned at seed: costed at its own
    // model's rate regardless of what `/model` does meanwhile.
    pricing: Pricing,
}

impl Tally {
    /// Start a run's counts afresh and pin the rate it is priced at. `/model`
    /// is answered at once, so a turn that began on one model and ends after a
    /// switch is still costed at what the model that ran it charges.
    pub fn seed(&mut self, pricing: Pricing) {
        *self = Self::default();
        self.pricing = pricing;
    }

    /// State the rate from outside a run: a surface that reads a receiver of
    /// its own knows when the model changed and the lane does not.
    pub fn set_pricing(&mut self, pricing: Pricing) {
        self.pricing = pricing;
    }

    /// Read one event for whatever number it carries, priced at the rate this
    /// run was seeded with.
    pub fn on(&mut self, event: &agent::Event) {
        match event {
            // A retry sends a second one for the same turn: the count it
            // carries replaces the abandoned attempt's rather than joining it.
            agent::Event::Usage(usage) => self.turn = *usage,
            agent::Event::TurnEnd {
                usage, generating, ..
            } => {
                self.settled.add(usage, self.pricing.cost(usage));
                self.turn = Usage::default();
                if let Some(took) = generating {
                    self.generated.0 += usage.output;
                    self.generated.1 += *took;
                }
            }
            agent::Event::Context { used, window } => self.ctx = Some((*used, *window)),
            agent::Event::Compacted(report) => {
                self.compactions += 1;
                self.compacted = Some(*report);
                // The last count is of a transcript that is no longer there;
                // only the next request's count says what this one occupies.
                self.ctx = None;
            }
            agent::Event::Refit(why) => self.refits.push(why.clone()),
            agent::Event::Warning(gap) => self.gaps.push(gap.clone()),
            // Replaces the running count (not adds): a compaction summary
            // is a paid call with no event of its own, only caught here.
            agent::Event::Done {
                usage,
                ctx,
                compactions,
                ..
            } => {
                self.settled = Totals {
                    usage: *usage,
                    cost: self.pricing.cost(usage),
                };
                self.turn = Usage::default();
                if ctx.is_some() {
                    self.ctx = *ctx;
                }
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
            cached: cached(&run.usage),
            cost: run.cost,
            ctx: self.ctx,
            speed: self.speed(),
            compactions: self.compactions,
            refit: !self.refits.is_empty(),
            queued,
            model: model.to_string(),
            worktree: worktree.map(str::to_string),
        }
    }

    // Output tokens a second over the time spent streaming them.
    fn speed(&self) -> Option<f64> {
        let (tokens, took) = self.generated;
        (!took.is_zero()).then(|| tokens as f64 / took.as_secs_f64())
    }

    /// The last compaction of this run, or of a `/compact` since.
    pub fn compacted(&self) -> Option<&agent::Report> {
        self.compacted.as_ref()
    }

    /// Why this run resent smaller, in the order it happened.
    pub fn refits(&self) -> &[String] {
        &self.refits
    }

    /// What the endpoint left out of this run's replies.
    pub fn gaps(&self) -> &[String] {
        &self.gaps
    }

    /// The context used against the window, as the last event said.
    pub fn ctx(&self) -> Option<(usize, usize)> {
        self.ctx
    }

    // What this run has spent so far: the turns that have reported plus the
    // one in flight.
    fn run_spend(&self) -> Totals {
        let mut t = self.settled;
        t.usage.add(&self.turn);
        t
    }
}

// Over the same sums `input` and the cost are, so the three agree.
fn cached(u: &Usage) -> Option<u64> {
    let sent = u.prompt();
    (sent > 0).then(|| u.cache_read * 100 / sent)
}

/// A compaction's report in one line, for `/status` and the surfaces that
/// print it as it happens.
pub fn compaction_line(r: &agent::Report) -> String {
    let mut parts = Vec::new();
    if r.superseded > 0 {
        parts.push(format!("{} superseded", r.superseded));
    }
    if r.aged_out > 0 {
        parts.push(format!("{} aged out", r.aged_out));
    }
    if r.args_taken > 0 {
        parts.push(format!("{} arguments taken", r.args_taken));
    }

    if r.notices_pruned > 0 {
        parts.push(format!("{} notices pruned", r.notices_pruned));
    }
    if r.dropped > 0 {
        let how = if r.summarized {
            "summarized"
        } else {
            "dropped"
        };
        parts.push(format!("{} messages {how}", r.dropped));
    }
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!("{}{}", icons::PART_SEP, parts.join(", "))
    };
    let warn = if r.still_over {
        format!("{}still over budget", icons::PART_SEP)
    } else {
        String::new()
    };
    format!("compacted{detail}{warn}")
}

// Takes the three pieces rather than a config entry: the running model may
// never have had one — a passed-through name has no entry to read.
pub(super) fn summary(format: &str, window: u32, p: &llm::model::Pricing) -> String {
    let mut parts = vec![format.to_string(), llm::figures::size(u64::from(window))];
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
        t.seed(priced());
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
        assert_eq!((mid.input, mid.output), (100, 20));

        t.on(&agent::Event::TurnEnd {
            usage: usage(100, 30),
            text: String::new(),
            generating: Some(Duration::from_secs(2)),
        });
        t.on(&agent::Event::TurnStart { turn: 2 });
        t.on(&agent::Event::Usage(usage(400, 7)));
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output), (500, 37));
        // 100 in at 10/mtok, 30 out at 100/mtok. The turn in flight is not
        // priced until it ends, so it contributes to neither figure.
        assert_eq!(s.cost, 0.004);
        // Speed is the finished turn's: 30 out over its 2s of streaming.
        assert_eq!(s.speed, Some(15.0));
    }

    // The cache share is weighted over the run's requests, not the latest
    // one's, and a mid-stream report of output alone leaves it be.
    #[test]
    fn the_cache_share_is_the_runs() {
        let mut t = seeded();
        assert_eq!(t.snapshot("m", None, None, 0).cached, None);
        let hit = |input, cache_read| Usage {
            input,
            cache_read,
            ..Default::default()
        };
        t.on(&agent::Event::TurnEnd {
            usage: hit(1_000, 0),
            text: String::new(),
            generating: None,
        });
        t.on(&agent::Event::TurnEnd {
            usage: hit(51, 27_800),
            text: String::new(),
            generating: None,
        });
        // 27,800 of 28,851 prompt tokens.
        assert_eq!(t.snapshot("m", None, None, 0).cached, Some(96));
        t.on(&agent::Event::Usage(usage(0, 12)));
        assert_eq!(t.snapshot("m", None, None, 0).cached, Some(96));
    }

    // A run's own total replaces the running one (same turns counted once):
    // an automatic compaction's summary is in it but in no event at all.
    #[test]
    fn a_finished_run_states_the_total_rather_than_adding_to_it() {
        let mut t = seeded();
        t.on(&agent::Event::TurnStart { turn: 1 });
        t.on(&agent::Event::TurnEnd {
            usage: usage(8_400, 390),
            text: String::new(),
            generating: None,
        });
        t.on(&agent::Event::Done {
            turns: 2,
            usage: usage(8_400, 390),
            ctx: Some((72_400, 114_000)),
            compactions: 1,
        });
        let s = t.snapshot("m", None, None, 0);
        // The run's word replaces the running tally rather than joining it:
        // the same turns counted twice would double every number here.
        assert_eq!((s.input, s.output), (8_400, 390));
        assert_eq!(s.cost, 0.123);
        assert_eq!(s.ctx, Some((72_400, 114_000)));
        assert_eq!(s.compactions, 1);
        // Nothing streamed, so no time to divide by.
        assert_eq!(s.speed, None);
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
            ctx: None,
            compactions: 0,
        });
        let s = t.snapshot("m", None, None, 0);
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
