//! What the two status lines say, and which parts each one says it with.
//!
//! The live line is repainted while a turn runs; the done line is the last of
//! those frames, kept in the scrollback with the run's own final word written
//! over it. Both read one `Tally` through one `Snapshot`, so agreement between
//! them is structural rather than two counts that happen to match.
//!
//! Both say the run in flight, and nothing before it: a session millions of
//! tokens deep would otherwise never let the line read as what this answer
//! cost. The session's own running total is `Tally::session`, and `/status` is
//! the one place that reads it.

use std::time::Duration;

use brain::stream::Usage;
use brain::totals::Totals;
use serde::{Deserialize, Serialize};

pub const SPIN: Duration = Duration::from_millis(90);

fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// Every value a status line can draw on, as far as it is known right now.
///
/// The spend figures are the run in flight's own, not the session's: what this
/// answer has cost since it was submitted, which is what the line is read for.
///
/// A zero count is the provider having stated nothing, and reads as a dash;
/// every other zero drops its segment rather than standing in for a
/// measurement. Owned rather than borrowed: a finished run's snapshot outlives
/// the lane it was taken from, sitting in the scrollback until the screen is
/// rebuilt.
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

/// What the events have said was spent, kept as they arrive.
///
/// One per surface. It holds two figures over one set of events: the run in
/// flight, which the lines draw, and the session it is part of, which
/// `session` hands to `/status`.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    // What earlier runs of this session had spent when this one started.
    // The surface injects it; absent it is zero, and `session` reads what
    // this run spent alone — which is what a pipe sees.
    base: Totals,
    // Turns of this run that have reported, and what they were priced at.
    settled: Totals,
    // The turn in flight, as far as the provider has said. Superseded rather
    // than added to when its `TurnEnd` lands, or the input would count twice.
    // Unpriced: a turn is costed when it ends.
    turn: Usage,
    turns: usize,
    ctx: Option<(usize, usize)>,
    compactions: usize,
}

impl Tally {
    /// Start a run's counts with the session's earlier runs already spent.
    pub fn seed(&mut self, base: Totals) {
        *self = Self::default();
        self.base = base;
    }

    /// Read one event for whatever number it carries.
    pub fn on(&mut self, event: &agent::Event) {
        match event {
            agent::Event::TurnStart { turn } => self.turns = *turn,
            // A retry sends a second one for the same turn: the count it
            // carries replaces the abandoned attempt's rather than joining it.
            agent::Event::Usage(usage) => self.turn = *usage,
            agent::Event::TurnEnd { usage, cost } => {
                self.settled.add(usage, *cost);
                self.turn = Usage::default();
            }
            agent::Event::Context { used, budget } => self.ctx = Some((*used, *budget)),
            agent::Event::Compacted(_) => self.compactions += 1,
            // The run's own word, which replaces the running count rather
            // than adding to it. Not merely the same sum: an automatic
            // compaction's summary is a call the run pays for and no event
            // states, so only the total that comes home includes it.
            agent::Event::Done {
                turns,
                usage,
                cost,
                ctx,
                compactions,
            } => {
                self.settled = Totals {
                    usage: *usage,
                    cost: *cost,
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
    /// for here: a pipe has no clock and no queue, and neither is a number the
    /// run reports.
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

/// One part of a status line. These are the names the config lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Elapsed,
    InOut,
    Cache,
    Cost,
    Turns,
    Ctx,
    Compacted,
    Queued,
    Model,
    Worktree,
}

impl Segment {
    /// What this part reads as, or None when the run has nothing to say for it.
    pub fn render(self, s: &Snapshot) -> Option<String> {
        Some(match self {
            Segment::Elapsed => elapsed(s.elapsed?),
            // Dashes say "a turn ran and the host stated nothing". A run
            // nothing has been spent on yet — no turn has stated a count —
            // drops the part instead of showing a row of zeros.
            Segment::InOut if s.turns == 0 && s.input == 0 && s.output == 0 => return None,
            Segment::InOut => brain::count::in_out(s.input, s.output),
            Segment::Cache if s.cache_read == 0 => return None,
            Segment::Cache => format!("{} cached", brain::count::short(s.cache_read)),
            // An unpriced model reports no cost rather than $0.
            Segment::Cost if s.cost <= 0.0 => return None,
            Segment::Cost => format!("${:.4}", s.cost),
            Segment::Turns if s.turns == 0 => return None,
            Segment::Turns => format!("{} turns", s.turns),
            // Both numbers rather than the share between them: a percentage of
            // a million-token window reads as 0% for most of a session.
            Segment::Ctx => match s.ctx? {
                (_, 0) => return None,
                (used, budget) => format!(
                    "ctx {}/{}",
                    brain::count::short(used as u64),
                    brain::count::short(budget as u64)
                ),
            },
            Segment::Compacted if s.compactions == 0 => return None,
            Segment::Compacted => format!("compacted {}×", s.compactions),
            Segment::Queued if s.queued == 0 => return None,
            Segment::Queued => format!("{} queued", s.queued),
            Segment::Model if s.model.is_empty() => return None,
            Segment::Model => s.model.clone(),
            // Absent in the repository's own checkout, so the line reads as it
            // always did for anyone not using worktrees.
            Segment::Worktree => s.worktree.clone()?,
        })
    }
}

/// The parts that have something to say, in the order asked for.
pub fn parts(segments: &[Segment], s: &Snapshot) -> Vec<String> {
    segments.iter().filter_map(|seg| seg.render(s)).collect()
}

/// Those parts as one line.
pub fn line(segments: &[Segment], s: &Snapshot) -> String {
    parts(segments, s).join(crate::icons::PART_SEP)
}

/// Which parts each line shows, as the config states it. An absent list is the
/// default one, so a file that names neither reads as the shipped layout.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lines {
    #[serde(default = "default_live")]
    pub live: Vec<Segment>,
    #[serde(default = "default_done")]
    pub done: Vec<Segment>,
}

impl Default for Lines {
    fn default() -> Self {
        Self {
            live: default_live(),
            done: default_done(),
        }
    }
}

/// The live line when the config names nothing: elapsed, the counts, the cache
/// read so far, context, queued work, and the worktree.
pub fn default_live() -> Vec<Segment> {
    vec![
        Segment::Elapsed,
        Segment::InOut,
        Segment::Cache,
        Segment::Ctx,
        Segment::Queued,
        Segment::Worktree,
    ]
}

/// The same for the done line — `turns · in/out · cached · $cost` as it stood,
/// with context and compaction added.
pub fn default_done() -> Vec<Segment> {
    vec![
        Segment::Turns,
        Segment::InOut,
        Segment::Cache,
        Segment::Ctx,
        Segment::Compacted,
        Segment::Cost,
        Segment::Worktree,
    ]
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

    // The running count is the turns that have reported plus the one in
    // flight, and a turn's own report supersedes what it had said so far.
    #[test]
    fn a_tally_carries_the_finished_turns_and_the_one_in_flight() {
        let mut t = Tally::default();
        t.on(&agent::Event::TurnStart { turn: 1 });
        t.on(&agent::Event::Usage(usage(100, 5)));
        t.on(&agent::Event::Usage(usage(100, 20)));
        let mid = t.snapshot("m", None, None, 0);
        assert_eq!((mid.input, mid.output, mid.turns), (100, 20, 1));

        t.on(&agent::Event::TurnEnd {
            usage: usage(100, 30),
            cost: 0.001,
        });
        t.on(&agent::Event::TurnStart { turn: 2 });
        t.on(&agent::Event::Usage(usage(400, 7)));
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output, s.turns), (500, 37, 2));
        assert_eq!(s.cost, 0.001);
    }

    // A run's own total replaces the running one rather than joining it: the
    // two count the same turns, and an automatic compaction's summary is in
    // the first and in no event at all.
    #[test]
    fn a_finished_run_states_the_total_rather_than_adding_to_it() {
        let mut t = Tally::default();
        t.on(&agent::Event::TurnStart { turn: 1 });
        t.on(&agent::Event::TurnEnd {
            usage: usage(8_400, 390),
            cost: 0.0012,
        });
        t.on(&agent::Event::Done {
            turns: 2,
            usage: usage(8_400, 390),
            cost: 0.0031,
            ctx: (72_400, 114_000),
            compactions: 1,
        });
        let s = t.snapshot("m", None, None, 0);
        // The run's word replaces the running tally rather than joining it:
        // the same turns counted twice would double every number here.
        assert_eq!(s.turns, 2);
        assert_eq!((s.input, s.output), (8_400, 390));
        assert_eq!(s.cost, 0.0031);
        assert_eq!(s.ctx, Some((72_400, 114_000)));
        assert_eq!(s.compactions, 1);
    }

    // The tally is seeded with what the session spent before this run, and
    // that figure stays off the lines: a line reads the run in flight, and
    // `session` is where the whole of it is asked for.
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
        let mut t = Tally::default();
        t.seed(base);
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
            cost: 0.003,
            ctx: (72_400, 114_000),
            compactions: 0,
        });
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output), (200, 30));
        assert_eq!(s.cost, 0.003);
        let session = t.session();
        assert_eq!((session.usage.input, session.usage.output), (10_200, 4_030));
        assert_eq!(session.cost, 0.023);
    }

    // A `!` command and a compaction begin no turn, and a host that reports
    // nothing has none to report: nothing may mint counts out of either.
    #[test]
    fn no_event_mints_counts() {
        let quiet = Tally::default().snapshot("m", None, None, 0);
        assert_eq!((quiet.input, quiet.output), (0, 0));

        let mut t = Tally::default();
        t.on(&agent::Event::TurnStart { turn: 1 });
        let started = t.snapshot("m", None, None, 0);
        assert_eq!((started.input, started.output), (0, 0));

        t.on(&agent::Event::Done {
            turns: 3,
            usage: Usage::default(),
            cost: 0.0,
            ctx: (0, 0),
            compactions: 0,
        });
        let s = t.snapshot("m", None, None, 0);
        assert_eq!(s.turns, 3);
        assert_eq!((s.input, s.output), (0, 0));
    }

    // Every field the events can fill is filled here, and the four they
    // cannot are the surface's own — there is nowhere else for a caller to
    // patch one in afterwards.
    #[test]
    fn a_snapshot_asks_for_what_no_event_states() {
        let s = Tally::default().snapshot("sonnet", Some("f1"), Some(Duration::from_secs(3)), 2);
        assert_eq!(s.model, "sonnet");
        assert_eq!(s.worktree.as_deref(), Some("f1"));
        assert_eq!(s.elapsed, Some(Duration::from_secs(3)));
        assert_eq!(s.queued, 2);
    }
}
