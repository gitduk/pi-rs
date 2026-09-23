//! What each part of a status line reads as, and how the parts make one line.
//!
//! Which parts a line shows is the config's, so the vocabulary is below this
//! (`store/status.rs`); what is here draws them. The live line is repainted
//! while a turn runs; the done line is the last of those frames, kept in the
//! scrollback with the run's own final word written over it. Both read one
//! `Tally` through one `Snapshot`, so agreement between them is structural
//! rather than two counts that happen to match.
//!
//! Both say the run in flight, and nothing before it: a session millions of
//! tokens deep would otherwise never let the line read as what this answer
//! cost.

use std::time::Duration;

use crate::app::meter::Snapshot;
use crate::store::status::Segment;

pub const SPIN: Duration = Duration::from_millis(90);

impl Segment {
    /// What this part reads as, or None when the run has nothing to say for it.
    pub fn render(self, s: &Snapshot) -> Option<String> {
        Some(match self {
            Segment::Elapsed => llm::count::elapsed(s.elapsed?),
            // Dashes say "a turn ran and the host stated nothing". A run
            // nothing has been spent on yet — no turn has stated a count —
            // drops the part instead of showing a row of zeros.
            Segment::InOut if s.turns == 0 && s.input == 0 && s.output == 0 => return None,
            Segment::InOut => llm::count::in_out(s.input, s.output),
            Segment::Cache if s.cache_read == 0 => return None,
            Segment::Cache => format!("{} cached", llm::count::short(s.cache_read)),
            // An unpriced model reports no cost rather than $0.
            Segment::Cost if s.cost <= 0.0 => return None,
            Segment::Cost => format!("${:.4}", s.cost),
            // Both numbers rather than the share between them: a percentage of
            // a million-token window reads as 0% for most of a session.
            Segment::Ctx => match s.ctx? {
                (_, 0) => return None,
                (used, budget) => format!(
                    "ctx {}/{}",
                    llm::count::short(used as u64),
                    llm::count::short(budget as u64)
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
