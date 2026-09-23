//! Where a mouse event landed, and what that row means: the frame's four
//! regions, and the row under the pointer.

use ratatui::layout::{Constraint, Layout, Rect};

use super::Ui;
use super::view::View;

impl Ui {
    // Nudge the scrolled history window by `step` rows, up or down.
    pub(super) fn scroll_view(&mut self, view: &mut View, up: bool, step: usize) {
        view.surface.scroll = if up {
            view.surface.scroll.saturating_add(step)
        } else {
            view.surface.scroll.saturating_sub(step)
        };
        // The row under the mouse changed: the old hover index no longer
        // names the screen position, so drop it until the next move.
        self.hovered_scrollback = None;
    }

    // What the row at this screen row sits on: the history region names
    // the frame, the target names the block inside it.
    fn target_at(&self, row: u16) -> Option<Target> {
        self.row_targets
            .get(row.checked_sub(self.regions.history.y)? as usize)
            .copied()
    }

    /// The scrollback row the mouse is over, if the cursor is on its text.
    fn hovered_row(&self, view: &View, col: u16, row: u16) -> Option<usize> {
        let Target::Scrollback(idx) = self.target_at(row)? else {
            return None;
        };
        let row = view.surface.scrollback.get(idx)?;
        if !row.is_expandable() {
            return None;
        }
        // The mouse must cover the row's own text, not the empty rest of the
        // row: the click that expands it lands on the head line only.
        let first = row.line(0, &self.paint, self.screen.usable()).0;
        ((col as usize) < first.width()).then_some(idx)
    }

    pub(super) fn on_mouse_move(&mut self, view: &mut View, col: u16, row: u16) {
        self.hovered_scrollback = self.hovered_row(view, col, row);
    }

    pub(super) fn on_mouse_click(&mut self, view: &mut View, col: u16, row: u16) {
        match self.target_at(row) {
            // The pending batch opens and closes where it stands: the live
            // rows are rebuilt every frame, so the flip is all it takes.
            Some(Target::PendingTools) => self.live_tools_shown = !self.live_tools_shown,
            _ => {
                if let Some(idx) = self.hovered_row(view, col, row)
                    && view
                        .surface
                        .scrollback
                        .get_mut(idx)
                        .is_some_and(|r| r.toggle_expand())
                {
                    view.surface.counted = None;
                }
            }
        }
    }

    // A page keeps 4 rows of context at the edge, the way the upstream pi
    // TUI does (`Math.max(1, viewportHeight - 4)`).
    pub(super) fn page_scroll_step(&self) -> usize {
        (self.screen.height as usize).saturating_sub(4).max(1)
    }

    pub(super) fn half_scroll_step(&self) -> usize {
        ((self.screen.height as usize) / 2).max(1)
    }
}

// What one rendered row of the history area sits on, as a click sees it:
// which block, named the only way a block in that region can be — a
// scrollback row by index, the live region's pending-call rows, or nothing.
#[derive(Clone, Copy, Debug)]
pub(super) enum Target {
    None,
    Scrollback(usize),
    PendingTools,
}

// The frame's four regions, laid out once and drawn by name. The only place
// the vertical arrangement is stated; everything else reads the rects back.
#[derive(Clone, Copy, Default)]
pub(super) struct Regions {
    // The scrolled transcript, the live region's rows included.
    pub(super) history: Rect,
    // The completion list, the rewind selector, or the open panel over it.
    pub(super) menu: Rect,
    // The bar: the checkouts, or the flash that took its row.
    pub(super) bar: Rect,
    // The input line, pinned to the bottom.
    pub(super) editor: Rect,
}

// The menu row's left column for an @ path: the file's own name, `/` when
// it is a directory the walk can descend into.
pub(super) fn at_row_name(path: &str, dir: bool) -> String {
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path);
    if dir {
        format!("{name}/")
    } else {
        name.to_string()
    }
}

impl Regions {
    pub(super) fn layout(area: Rect, menu_h: u16, bar_h: u16, editor_h: u16) -> Self {
        let chunks = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(menu_h),
            Constraint::Length(bar_h),
            Constraint::Length(editor_h),
        ])
        .split(area);
        Self {
            history: chunks[0],
            menu: chunks[1],
            bar: chunks[2],
            editor: chunks[3],
        }
    }
}
