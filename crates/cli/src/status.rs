//! What the two status lines say, and which parts each one says it with.
//!
//! The live line is repainted while a turn runs; the done line is printed when
//! one ends and stays in the scrollback. Both draw on the same values, so the
//! parts are named once here and each surface takes the ones it was configured
//! for.

use std::time::Duration;

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
/// `None` leaves its segment out rather than standing in for it; a zero count
/// is the provider having stated nothing, and reads as a dash.
#[derive(Debug, Default, Clone, Copy)]
pub struct Snapshot<'a> {
    pub elapsed: Option<Duration>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cost: Option<f64>,
    pub turns: Option<usize>,
    /// Used against usable, in tokens. The denominator is the budget, not the
    /// window, so 100% is where compaction fires rather than where it refuses.
    pub ctx: Option<(usize, usize)>,
    pub compactions: usize,
    pub queued: usize,
    pub model: &'a str,
    /// The worktree the session is working in, or None in the repository's own
    /// checkout.
    pub worktree: Option<&'a str>,
}

impl Snapshot<'_> {
    /// What a finished run reports about itself. A surface fills in whatever it
    /// knows beyond that — the model, how long it took — before rendering.
    pub fn of_done(event: &agent::Event) -> Option<Self> {
        let agent::Event::Done {
            turns,
            usage,
            cost,
            ctx,
            compactions,
        } = event
        else {
            return None;
        };
        Some(Self {
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cost: Some(*cost),
            turns: Some(*turns),
            ctx: Some(*ctx),
            compactions: *compactions,
            ..Self::default()
        })
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
    pub fn render(self, s: &Snapshot<'_>) -> Option<String> {
        Some(match self {
            Segment::Elapsed => elapsed(s.elapsed?),
            Segment::InOut => crate::render::in_out(s.input, s.output),
            Segment::Cache if s.cache_read == 0 => return None,
            Segment::Cache => format!("{} cached", crate::render::short(s.cache_read)),
            Segment::Cost => match s.cost? {
                // An unpriced model reports no cost rather than $0.
                c if c <= 0.0 => return None,
                c => format!("${c:.4}"),
            },
            Segment::Turns => format!("{} turns", s.turns?),
            // Both numbers rather than the share between them: a percentage of
            // a million-token window reads as 0% for most of a session.
            Segment::Ctx => match s.ctx? {
                (_, 0) => return None,
                (used, budget) => format!(
                    "ctx {}/{}",
                    crate::render::short(used as u64),
                    crate::render::short(budget as u64)
                ),
            },
            Segment::Compacted if s.compactions == 0 => return None,
            Segment::Compacted => format!("compacted {}×", s.compactions),
            Segment::Queued if s.queued == 0 => return None,
            Segment::Queued => format!("{} queued", s.queued),
            Segment::Model if s.model.is_empty() => return None,
            Segment::Model => s.model.to_string(),
            // Absent in the repository's own checkout, so the line reads as it
            // always did for anyone not using worktrees.
            Segment::Worktree => s.worktree?.to_string(),
        })
    }
}

/// The parts that have something to say, in the order asked for.
pub fn parts(segments: &[Segment], s: &Snapshot<'_>) -> Vec<String> {
    segments.iter().filter_map(|seg| seg.render(s)).collect()
}

/// Those parts as one line.
pub fn line(segments: &[Segment], s: &Snapshot<'_>) -> String {
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

    fn full() -> Snapshot<'static> {
        Snapshot {
            elapsed: Some(Duration::from_secs(125)),
            input: 8_400,
            output: 390,
            cache_read: 41_200,
            cost: Some(0.0012),
            turns: Some(2),
            ctx: Some((72_400, 114_000)),
            compactions: 2,
            queued: 1,
            model: "deepseek-v4-flash",
            worktree: Some("feature-one"),
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
            cost: Some(0.0),
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
    #[test]
    fn a_finished_run_reads_as_the_line_it_ends_with() {
        let e = agent::Event::Done {
            turns: 2,
            usage: brain::stream::Usage {
                input: 8_400,
                output: 390,
                ..Default::default()
            },
            cost: 0.0012,
            ctx: (72_400, 114_000),
            compactions: 0,
        };
        let snap = Snapshot::of_done(&e).expect("a done event");
        assert_eq!(
            line(&default_done(), &snap),
            "2 turns · 8.4k in / 390 out · ctx 72.4k/114.0k · $0.0012"
        );
    }

    // A host that reported none of its usage: the line says the turns and shows
    // the gaps as dashes rather than hiding them.
    #[test]
    fn a_run_the_host_said_nothing_about_still_shows_the_dashes() {
        let e = agent::Event::Done {
            turns: 3,
            usage: brain::stream::Usage::default(),
            cost: 0.0,
            ctx: (0, 0),
            compactions: 0,
        };
        let snap = Snapshot::of_done(&e).expect("a done event");
        assert_eq!(
            line(&[Segment::Turns, Segment::InOut], &snap),
            "3 turns · - in / - out"
        );
    }

    #[test]
    fn only_a_done_event_makes_a_snapshot() {
        assert!(Snapshot::of_done(&agent::Event::TurnStart { turn: 1 }).is_none());
    }

    #[test]
    fn the_names_are_what_the_config_writes() {
        let names: Vec<Segment> =
            serde_json::from_str(r#"["elapsed","in_out","ctx","compacted","model"]"#).unwrap();
        assert_eq!(
            names,
            vec![
                Segment::Elapsed,
                Segment::InOut,
                Segment::Ctx,
                Segment::Compacted,
                Segment::Model
            ]
        );
    }
}
