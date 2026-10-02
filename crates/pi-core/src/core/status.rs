//! What a status line reads: the parts of a lane a reader is shown, and the
//! numbers behind them.
//!
//! The vocabulary is `store/status.rs` and the drawing is `ui/status.rs`; this
//! is where the values come from.

use agent::Totals;

use super::Core;
use crate::store::journal;

impl Core {
    // Instruction files are named, not quoted — `standing` carries them whole.
    // Spend reads the live tally, not settled totals, so a run in flight counts.
    pub(super) fn status_lines(&self) -> Vec<String> {
        let lane = self.lane();
        let mut out = standing_head(&lane.resolved().standing);
        out.extend(lane.resolved().endpoint.clone());
        if !lane.resolved().context.is_empty() {
            out.push("context:".into());
            out.extend(lane.resolved().context.iter().map(|c| format!("- {c}")));
        }
        out.push(match journal::path() {
            Some(p) => format!("journal: {}", p.display()),
            None => "journal: not recording — PI_LOG is off, or it would not open".into(),
        });
        out.push(format!(
            "session: {}",
            self.store.path_of(lane.root(), lane.id()).display()
        ));
        // Left out until something has been spent: a session nothing has been
        // asked of yet has no figure, and a row of dashes is not one.
        let spent = lane.tally().session();
        if spent != Totals::default() {
            out.push(format!("spent: {}", crate::text::spent(&spent)));
        }
        out
    }
    /// What a named lane's transcript occupies now, for the line that says why
    /// there was nothing to compact. Zero while a run has it.
    pub fn tokens_now_at(&self, at: usize) -> usize {
        let Some(lane) = self.lanes.get(at) else {
            return 0;
        };
        lane.session().map_or(0, |s| {
            llm::estimate::tokens(&s.context(), lane.agent().spec())
        })
    }
}

// What becomes of prior-turn reasoning once another model reads it — the
// signed path is out, so it's one of these three.
pub(super) fn demotion(replay: llm::model::ReplayThinking) -> &'static str {
    use llm::model::ReplayThinking as R;
    match replay {
        R::Tagged => "reasoning from the earlier turns replays wrapped in <think> tags",
        R::Off => "reasoning from the earlier turns is dropped rather than replayed",
    }
}

// Whether the transcript holds prior-turn reasoning — worth saying at a
// switch, since it's the one part of history that doesn't survive intact.
pub(super) fn carries_reasoning(session: &agent::session::Session) -> bool {
    // The view, not every entry: what compaction has already dropped is not
    // going to reach the new model in any form, demoted or otherwise.
    session.view().iter().any(|s| {
        s.entry().blocks().is_some_and(|bs| {
            bs.iter()
                .any(|b| matches!(b, llm::message::AssistantContent::Reasoning(_)))
        })
    })
}

// The system prompt's tail up to the first instruction file. Split on the
// tag, not a count, so a field added to the head shows up automatically.
fn standing_head(standing: &str) -> Vec<String> {
    standing
        .split("<instructions")
        .next()
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}
