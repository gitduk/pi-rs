//! What the two status lines say, and which parts each one says it with.
//!
//! The live line is repainted while a turn runs; the done line is the last of
//! those frames, kept in the scrollback with the run's own final word written
//! over it. Both read one `Tally` through one `Snapshot`, so agreement between
//! them is structural rather than two counts that happen to match.

use std::time::Duration;

use brain::stream::Usage;
use brain::totals::Totals;
use serde::{Deserialize, Serialize};

pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
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

/// What the events have said a run has spent, kept as they arrive.
///
/// One per surface. Every number a status line shows comes from here, which is
/// what makes the live line and the line the run ends on the same reading at
/// two moments rather than two tallies kept in step by hand.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    /// Turns that have reported, and what they were priced at.
    settled: Totals,
    /// The turn in flight, as far as the provider has said. Superseded rather
    /// than added to when its `TurnEnd` lands, or the input would count twice.
    /// Unpriced: a turn is costed when it ends.
    turn: Usage,
    turns: usize,
    ctx: Option<(usize, usize)>,
    compactions: usize,
}

impl Tally {
    /// Start a run from nothing; the clock and the queue belong to the surface.
    pub fn clear(&mut self) {
        *self = Self::default();
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
            // The run's own word, which replaces the running count rather than
            // adding to it. It is not merely the same sum: an automatic
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
        Snapshot {
            elapsed,
            input: self.settled.usage.input + self.turn.input,
            output: self.settled.usage.output + self.turn.output,
            cache_read: self.settled.usage.cache_read + self.turn.cache_read,
            cost: self.settled.cost,
            turns: self.turns,
            ctx: self.ctx,
            compactions: self.compactions,
            queued,
            model: model.to_string(),
            worktree: worktree.map(str::to_string),
        }
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
            // Dashes say "a turn ran and the host stated nothing". Work that
            // begins no turn at all — a `!` command, a compaction — has no
            // counts to be silent about, and a row of dashes under it reads as
            // a model call that cost nothing.
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
    parts(segments, s).join(" · ")
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

    fn full() -> Snapshot {
        Snapshot {
            elapsed: Some(Duration::from_secs(125)),
            input: 8_400,
            output: 390,
            cache_read: 41_200,
            cost: 0.0012,
            turns: 2,
            ctx: Some((72_400, 114_000)),
            compactions: 2,
            queued: 1,
            model: "deepseek-v4-flash".into(),
            worktree: Some("feature-one".into()),
        }
    }

    #[test]
    fn every_segment_has_a_wording() {
        let s = full();
        let all = [
            Segment::Elapsed,
            Segment::InOut,
            Segment::Cache,
            Segment::Cost,
            Segment::Turns,
            Segment::Ctx,
            Segment::Compacted,
            Segment::Queued,
            Segment::Model,
        ];
        assert_eq!(
            parts(&all, &s),
            vec![
                "2m05s",
                "8.4k in / 390 out",
                "41.2k cached",
                "$0.0012",
                "2 turns",
                "ctx 72.4k/114.0k",
                "compacted 2×",
                "1 queued",
                "deepseek-v4-flash",
            ]
        );
    }

    // What a pipe cannot say and a turn in flight does not know yet: the
    // segment goes rather than showing a zero that reads as a measurement.
    #[test]
    fn a_surface_with_nothing_to_say_drops_the_segment() {
        let s = Snapshot::default();
        let quiet = [
            Segment::Elapsed,
            Segment::Cache,
            Segment::Cost,
            Segment::Turns,
            Segment::Ctx,
            Segment::Compacted,
            Segment::Queued,
            Segment::Model,
            Segment::Worktree,
        ];
        assert!(parts(&quiet, &s).is_empty());
    }

    // The one exception: the counts are always shown, and the half the provider
    // has not stated is a dash.
    #[test]
    fn the_counts_stay_and_show_a_dash_for_the_unstated_half() {
        let s = Snapshot {
            input: 8_400,
            ..Snapshot::default()
        };
        assert_eq!(line(&[Segment::InOut], &s), "8.4k in / - out");
    }

    #[test]
    fn the_config_decides_the_order() {
        let s = full();
        assert_eq!(
            line(&[Segment::Cost, Segment::Turns], &s),
            "$0.0012 · 2 turns"
        );
    }

    #[test]
    fn a_budget_of_nothing_is_not_a_division() {
        let s = Snapshot {
            ctx: Some((100, 0)),
            ..Snapshot::default()
        };
        assert!(parts(&[Segment::Ctx], &s).is_empty());
    }

    #[test]
    fn an_unpriced_model_shows_no_cost() {
        let s = Snapshot {
            cost: 0.0,
            ..Snapshot::default()
        };
        assert!(parts(&[Segment::Cost], &s).is_empty());
    }

    #[test]
    fn the_default_live_line_shows_what_has_been_read_from_cache() {
        let s = full();
        assert_eq!(
            line(&default_live(), &s),
            "2m05s · 8.4k in / 390 out · 41.2k cached · ctx 72.4k/114.0k · 1 queued · feature-one"
        );
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Default::default()
        }
    }

    /// The running count is the turns that have reported plus the one in
    /// flight, and a turn's own report supersedes what it had said so far.
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

    /// A run's own total replaces the running one rather than joining it: the
    /// two count the same turns, and an automatic compaction's summary is in
    /// the first and in no event at all.
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
        assert_eq!(
            line(&default_done(), &s),
            "2 turns · 8.4k in / 390 out · ctx 72.4k/114.0k · compacted 1× · $0.0031"
        );
    }

    // A host that reported none of its usage: the line says the turns and shows
    // the gaps as dashes rather than hiding them.
    #[test]
    fn a_run_the_host_said_nothing_about_still_shows_the_dashes() {
        let mut t = Tally::default();
        t.on(&agent::Event::Done {
            turns: 3,
            usage: Usage::default(),
            cost: 0.0,
            ctx: (0, 0),
            compactions: 0,
        });
        let s = t.snapshot("", None, None, 0);
        assert_eq!(
            line(&[Segment::Turns, Segment::InOut], &s),
            "3 turns · - in / - out"
        );
    }

    /// A `!` command and a compaction begin no turn, so the counts have
    /// nothing to be silent about; a turn the host said nothing for still
    /// shows its dashes.
    #[test]
    fn work_that_begins_no_turn_shows_no_counts() {
        let quiet = Tally::default().snapshot("m", None, None, 0);
        assert!(parts(&[Segment::InOut], &quiet).is_empty());

        let mut t = Tally::default();
        t.on(&agent::Event::TurnStart { turn: 1 });
        let started = t.snapshot("m", None, None, 0);
        assert_eq!(line(&[Segment::InOut], &started), "- in / - out");
    }

    /// Every field the events can fill is filled here, and the four they
    /// cannot are the surface's own — there is nowhere else for a caller to
    /// patch one in afterwards.
    #[test]
    fn a_snapshot_asks_for_what_no_event_states() {
        let s = Tally::default().snapshot(
            "sonnet",
            Some("f1"),
            Some(Duration::from_secs(3)),
            2,
        );
        assert_eq!(s.model, "sonnet");
        assert_eq!(s.worktree.as_deref(), Some("f1"));
        assert_eq!(s.elapsed, Some(Duration::from_secs(3)));
        assert_eq!(s.queued, 2);
    }
}
