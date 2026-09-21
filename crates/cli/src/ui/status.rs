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

use serde::{Deserialize, Serialize};

use crate::run::meter::Snapshot;

pub const SPIN: Duration = Duration::from_millis(90);

fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
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
    parts(segments, s).join(crate::store::icons::PART_SEP)
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
