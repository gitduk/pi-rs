//! The rows a tool call occupies while it runs: the line it holds itself, and
//! the group that draws it when there is one to fold into.

use std::time::Instant;

use llm::message::ToolResult;
use ratatui::text::Line;

use super::row::{self, PendingTool, Row};
use super::scrollback::Folds;
use crate::render::{Paint, named};
use pi_core::core::tools::modifies;
use pi_core::store::icons;

// A tool call still running. Its line is drawn by the group it will fold
// into when there is one, and by the live block when there is not.
pub(super) struct RunTool {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) summary: String,
    pub(super) started: Instant,
    // Set when the call ended: the row its entry will adopt, parked until the
    // committed entries arrive — and what decides whether it can still fold.
    pub(super) done: Option<Row>,
}

fn out_secs(t: &RunTool, now: Instant) -> Option<u64> {
    let secs = now.saturating_duration_since(t.started).as_secs();
    (t.done.is_none() && secs >= SHOWN_AFTER).then_some(secs)
}

// A quick call is over before a count could be read, and one that shows `1s`
// only to vanish reads as a flicker.
const SHOWN_AFTER: u64 = 2;

// The call's line in the live block: mark, shimmering text, elapsed time.
// A modifying call never folds, so it stays drawn here after it ends.
pub(super) fn pending_line(now: Instant, tick: usize, t: &RunTool, paint: &Paint) -> Line<'static> {
    let ended = if modifies(&t.name) {
        None
    } else {
        t.done.as_ref().and_then(Row::ok)
    };
    let muted = |s: String| paint.span(&paint.theme.muted, s);
    let named = named(&t.name, &t.summary);
    match ended {
        // Mark leads, matching the row this call lands as once folded.
        Some(ok) => {
            let mark = if ok {
                icons::DONE_MARK
            } else {
                icons::FAIL_MARK
            };
            Line::from(muted(format!("{mark} {named}")))
        }
        None => {
            let mut spans = vec![muted(format!("{} ", row::call_frame(tick)))];
            spans.extend(row::shimmer(&named, tick, paint));
            spans.push(muted(row::out_for(out_secs(t, now))));
            Line::from(spans)
        }
    }
}

// Whether the group draws this call: true from when a foldable call
// starts until it ends badly — a failed call is on its way to its own line.
fn holds(t: &RunTool) -> bool {
    !modifies(&t.name) && t.done.as_ref().is_none_or(|row| row.ok() == Some(true))
}

// Foldable calls, shown from the start so the row doesn't visually jump
// when the result lands.
pub(super) fn held(tools: &[RunTool], now: Instant) -> Vec<PendingTool> {
    tools
        .iter()
        .filter(|t| holds(t))
        .map(|t| PendingTool {
            name: t.name.clone(),
            preview: t.summary.clone(),
            landed: t.done.is_some(),
            secs: out_secs(t, now),
        })
        .collect()
}

// Calls the live block draws: the complement of `held`'s foldable set.
pub(super) fn drawn(tools: &[RunTool], row_holds: bool) -> Vec<&RunTool> {
    tools.iter().filter(|t| !(row_holds && holds(t))).collect()
}

// Where a tool's row goes: a call that landed well and is not a write is a
// step of the group; anything else is a line of its own that ends it.
pub(super) fn push_tool_row(
    scrollback: &mut Vec<Row>,
    folds: &mut Folds,
    row: Row,
    result: &ToolResult,
) {
    let asked = folds.take_asked(&result.call);
    if let Some(name) = row.tool_name().filter(|&n| !modifies(n))
        && row.ok() == Some(true)
    {
        let tool = row::FoldedTool::new(
            name,
            row.tool_preview().unwrap_or_default(),
            &asked,
            &row::result_text(result),
        );
        folds.join(scrollback, row::Step::Tool(tool));
        return;
    }
    scrollback.push(row);
}
