//! Where a mouse event landed, and what that row means: the frame's four
//! regions, and the row under the pointer.

use ratatui::layout::{Constraint, Layout, Rect};

use super::Ui;
use super::view::View;

impl Ui {
    pub(super) fn scroll_view(&mut self, view: &mut View, up: bool, step: usize) {
        view.surface.scroll = if up {
            view.surface.scroll.saturating_add(step)
        } else {
            view.surface.scroll.saturating_sub(step)
        };
        // Screen positions shifted, so the old hover index is stale.
        self.hovered_scrollback = None;
    }

    fn target_at(&self, row: u16) -> Option<Target> {
        self.row_targets
            .get(row.checked_sub(self.regions.history.y)? as usize)
            .copied()
    }

    /// The scrollback row and line under the cursor, if the cursor is over
    /// that line's own text.
    fn hovered_row(&self, view: &View, col: u16, row: u16) -> Option<(usize, usize)> {
        let Target::Scrollback(idx, line) = self.target_at(row)? else {
            return None;
        };
        let row = view.surface.scrollback.get(idx)?;
        let width = self.screen.usable();
        let line = row.click_line(line, width)?;
        // Excludes the empty rest of the row past the text.
        let (text, border) = row.line(line, &self.paint, width);
        let start = border.map_or(0, |b| b.width());
        (start..start + text.width())
            .contains(&(col as usize))
            .then_some((idx, line))
    }

    pub(super) fn on_mouse_move(&mut self, view: &mut View, col: u16, row: u16) {
        self.hovered_scrollback = self.hovered_row(view, col, row);
    }

    pub(super) fn on_mouse_click(&mut self, view: &mut View, col: u16, row: u16) {
        match self.target_at(row) {
            // Live rows rebuild every frame, so a flag flip is enough.
            Some(Target::PendingTools) => self.live_tools_shown = !self.live_tools_shown,
            _ => {
                let width = self.screen.usable();
                let hit = self.hovered_row(view, col, row);
                if let Some((idx, code)) =
                    hit.and_then(|(idx, _)| Some((idx, view.surface.scrollback.get(idx)?.code()?)))
                {
                    let lines = code.lines().count();
                    let copied = crossterm::execute!(
                        std::io::stdout(),
                        crossterm::clipboard::CopyToClipboard::to_clipboard_from(code)
                    );
                    let said = match copied {
                        Ok(()) => format!("copied {}", super::row::count(lines, "line")),
                        Err(e) => format!("not copied: {e}"),
                    };
                    if view.surface.scrollback[idx].badged(&self.paint, width) {
                        self.copied = Some((idx, said, std::time::Instant::now()));
                    } else {
                        self.flash(said);
                    }
                } else if let Some((idx, line)) = hit
                    && view
                        .surface
                        .scrollback
                        .get_mut(idx)
                        .is_some_and(|r| r.toggle_at(line, width))
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

// What block a rendered history row belongs to, for click handling.
#[derive(Clone, Copy, Debug)]
pub(super) enum Target {
    None,
    Scrollback(usize, usize),
    PendingTools,
}

// The frame's four regions, laid out once and drawn by name. The only place
// the vertical arrangement is stated; everything else reads the rects back.
#[derive(Clone, Copy, Default)]
pub(super) struct Regions {
    // The scrolled transcript, the live region's rows included.
    pub(super) history: Rect,
    // The completion list, the rewind selector, or the open reply over it.
    pub(super) menu: Rect,
    // The bar: the checkouts, or the flash that took its row.
    pub(super) bar: Rect,
    // The input line, pinned to the bottom.
    pub(super) editor: Rect,
}

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
