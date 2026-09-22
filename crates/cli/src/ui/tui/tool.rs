//! The rows a tool call occupies while it runs: the spinner, the pending
//! line, and the one-line summary a finished call collapses to.

use super::row::{self, Row};
use crate::store::icons;

// A tool call still running, shown as one animated row in the live region
// until its result lands and the row scrolls up as a check or cross.
pub(super) struct RunTool {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) summary: String,
    // Set when the call ended: the row its entry will adopt, parked in the
    // live region until the committed entries arrive to check it against.
    pub(super) done: Option<Row>,
}

// The one row a still-running tool occupies. The frame is the animation;
// `ToolEnd` and `abandon_tools` replace the row with a final line.
fn tool_row(frame: usize, name: &str, summary: &str) -> String {
    let frame = icons::SPINNER_FRAMES[frame % icons::SPINNER_FRAMES.len()];
    format!("{frame} {}", row::named(name, summary))
}

// The live row one pending call occupies: the spinner while it runs, and
// once ended the mark it will land with — a modifying call never folds.
pub(super) fn pending_line(spinner: usize, t: &RunTool) -> String {
    let mark = if is_modifying_tool(&t.name) {
        None
    } else {
        t.done.as_ref().and_then(|row| row.ok()).map(|ok| {
            if ok {
                icons::DONE_MARK
            } else {
                icons::FAIL_MARK
            }
        })
    };
    let name = match mark {
        Some(mark) => format!("{mark} {}", t.name),
        None => t.name.clone(),
    };
    tool_row(spinner, &name, &t.summary)
}

pub(super) fn is_modifying_tool(name: &str) -> bool {
    matches!(name, "edit" | "write")
}

pub(super) fn push_tool_row(scrollback: &mut Vec<Row>, row: Row) {
    if let Some(name) = row.tool_name().filter(|&n| !is_modifying_tool(n))
        && row.ok() == Some(true)
    {
        let preview = row.tool_preview().unwrap_or_default().to_string();
        if let Some(last) = scrollback.last_mut()
            && last.push_tool(name.to_string(), preview.clone())
        {
            return;
        }
        scrollback.push(Row::tools_summary(row::FoldedTools::new(row::FoldedTool {
            name: name.to_string(),
            preview,
        })));
        return;
    }
    scrollback.push(row);
}
