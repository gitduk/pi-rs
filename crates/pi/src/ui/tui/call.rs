//! The rows a tool call occupies while it runs: the line it holds itself, and
//! the group that draws it when there is one to fold into.

use std::time::Instant;

use llm::message::ToolResult;
use ratatui::text::Line;

use super::row::{self, PendingTool, Row};
use super::scrollback::Folds;
use crate::store::icons;
use crate::ui::render::{Paint, named};

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

// A call not heard back from yet has been out this long, in whole seconds
// once there are enough of them to be worth saying.
fn out_secs(t: &RunTool, now: Instant) -> Option<u64> {
    let secs = now.saturating_duration_since(t.started).as_secs();
    (t.done.is_none() && secs >= SHOWN_AFTER).then_some(secs)
}

// A quick call is over before a count could be read, and one that shows `1s`
// only to vanish reads as a flicker.
const SHOWN_AFTER: u64 = 2;

// The line a call holds in the live block: its turning mark, its text
// shimmering and how long it has been out while it runs, and the mark it will
// land with once it has ended. A modifying call never folds, so it is drawn
// here even after it ends.
pub(super) fn pending_line(now: Instant, tick: usize, t: &RunTool, paint: &Paint) -> Line<'static> {
    let ended = if is_modifying_tool(&t.name) {
        None
    } else {
        t.done.as_ref().and_then(Row::ok)
    };
    let muted = |s: String| paint.span(&paint.theme.muted, s);
    let named = named(&t.name, &t.summary);
    match ended {
        // The mark leads, as it will in the row this call lands as: a frame
        // beside it would be animating a call that is over.
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

fn is_modifying_tool(name: &str) -> bool {
    name == toolbox::edit::Edit::NAME || name == toolbox::write::Write::NAME
}

// Whether the group draws for this call: the row is where a foldable
// call lands, so it is the row's from the moment it starts — and only while it
// can still land there. One that has ended badly is on its way to a line of
// its own, and the row must not name, count or mark what it will not keep.
fn holds(t: &RunTool) -> bool {
    !is_modifying_tool(&t.name) && t.done.as_ref().is_none_or(|row| row.ok() == Some(true))
}

// The calls the group draws, in the order they started, each with the
// state the row shows for it. A call lands as the foldable row it is drawn
// as, so showing it there from the start is what keeps the line from jumping
// up into the row when the result arrives.
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

// The calls in flight the live block draws: what is left when the group
// is drawing the foldable ones.
pub(super) fn drawn(tools: &[RunTool], row_holds: bool) -> Vec<&RunTool> {
    tools.iter().filter(|t| !(row_holds && holds(t))).collect()
}

// Where a tool's row goes: a read-only call that landed well is a step of
// the group, and anything else a line of its own that ends the group.
pub(super) fn push_tool_row(
    scrollback: &mut Vec<Row>,
    folds: &mut Folds,
    row: Row,
    result: &ToolResult,
) {
    let asked = folds.take_asked(&result.call);
    if let Some(name) = row.tool_name().filter(|&n| !is_modifying_tool(n))
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
