//! The rows a tool call occupies while it runs: the line it holds itself, and
//! the group that draws it when there is one to fold into.

use super::row::{self, PendingTool, Row};
use super::scrollback::Folds;
use crate::store::icons;
use crate::ui::render::named;

// A tool call still running. Its line is drawn by the group it will fold
// into when there is one, and by the live block when there is not.
pub(super) struct RunTool {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) summary: String,
    // Set when the call ended: the row its entry will adopt, parked until the
    // committed entries arrive — and what decides whether it can still fold.
    pub(super) done: Option<Row>,
}

// The one line a call still running wears: the frame leads, and the call's
// name and leading argument follow it.
fn tool_row(frame: usize, name: &str, summary: &str) -> String {
    let frame = icons::SPINNER_FRAMES[frame % icons::SPINNER_FRAMES.len()];
    format!("{frame} {}", crate::ui::render::named(name, summary))
}

// The line a call holds in the live block: the frame while it runs, and the
// mark it will land with once it has ended. A modifying call never folds, so
// it is drawn here even after it ends.
pub(super) fn pending_line(spinner: usize, t: &RunTool) -> String {
    let ended = if is_modifying_tool(&t.name) {
        None
    } else {
        t.done.as_ref().and_then(Row::ok)
    };
    match ended {
        // The mark leads, as it will in the row this call lands as: a frame
        // beside it would be animating a call that is over.
        Some(ok) => format!(
            "{} {}",
            if ok {
                icons::DONE_MARK
            } else {
                icons::FAIL_MARK
            },
            named(&t.name, &t.summary)
        ),
        None => tool_row(spinner, &t.name, &t.summary),
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
pub(super) fn held(tools: &[RunTool]) -> Vec<PendingTool> {
    tools
        .iter()
        .filter(|t| holds(t))
        .map(|t| PendingTool {
            name: t.name.clone(),
            preview: t.summary.clone(),
            landed: t.done.is_some(),
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
pub(super) fn push_tool_row(scrollback: &mut Vec<Row>, folds: &mut Folds, row: Row) {
    if let Some(name) = row.tool_name().filter(|&n| !is_modifying_tool(n))
        && row.ok() == Some(true)
    {
        let tool = row::FoldedTool {
            name: name.to_string(),
            preview: row.tool_preview().unwrap_or_default().to_string(),
        };
        folds.join(scrollback, row::Step::Tool(tool));
        return;
    }
    scrollback.push(row);
}
