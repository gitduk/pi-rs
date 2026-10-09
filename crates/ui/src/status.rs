//! Draws the status line from parts; which parts show is the config's, in
//! `pi_store::status`. Repainted while a turn runs, its last frame kept in
//! scrollback once it ends.
//!
//! Shows only the run in flight, not the whole session's total.

use std::time::Duration;

use pi_core::core::meter::Snapshot;
use pi_store::status::Segment;

pub const SPIN: Duration = Duration::from_millis(90);

/// What this part reads as, or None when the run has nothing to say for it.
pub fn render(segment: Segment, s: &Snapshot) -> Option<String> {
    Some(match segment {
        Segment::Elapsed => llm::figures::elapsed(s.elapsed?),
        // Dashes (elsewhere) mean "ran, but reported nothing"; this drops the
        // part instead when nothing has been spent on yet.
        Segment::InOut if s.turns == 0 && s.input == 0 && s.output == 0 => return None,
        Segment::InOut => llm::figures::in_out(s.input, s.output),
        // Shown whenever in/out is, a dash until a turn has streamed: a part
        // that came and went between frames would read as a glitch.
        Segment::Speed if s.turns == 0 && s.input == 0 && s.output == 0 => return None,
        Segment::Speed => match s.speed {
            Some(rate) => format!("{rate:.0} t/s"),
            None => "- t/s".to_string(),
        },
        Segment::Cache if s.cache_read == 0 => return None,
        Segment::Cache => format!("{} cached", llm::figures::short(s.cache_read)),
        // An unpriced model reports no cost rather than $0.
        Segment::Cost if s.cost <= 0.0 => return None,
        Segment::Cost => format!("${:.4}", s.cost),
        // Both numbers rather than the share between them: a percentage of
        // a million-token window reads as 0% for most of a session.
        Segment::Ctx => match s.ctx? {
            (_, 0) => return None,
            (used, window) => format!(
                "ctx {}/{}",
                llm::figures::short(used as u64),
                llm::figures::short(window as u64)
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

/// The parts that have something to say, in the order asked for.
pub fn parts(segments: &[Segment], s: &Snapshot) -> Vec<String> {
    segments.iter().filter_map(|&seg| render(seg, s)).collect()
}

/// Those parts as one line.
pub fn line(segments: &[Segment], s: &Snapshot) -> String {
    parts(segments, s).join(pi_store::icons::PART_SEP)
}
