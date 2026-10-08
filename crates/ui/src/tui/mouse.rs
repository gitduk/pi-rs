//! Where a mouse event landed, and what that row means: the frame's four
//! regions, and the row under the pointer.

use ratatui::layout::{Constraint, Layout, Rect};

use super::Ui;
use super::select::{self, Drawn, Point, Selection};
use super::ui::Press;
use super::view::View;

// Presses closer than this on one cell count as one double or triple click.
const MULTI_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

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
        let target = self.target_at(row)?;
        let Target::Scrollback(idx, line) = target else {
            return None;
        };
        // How many screen rows of this same line sit above the pointer: a
        // drawn block is one line, its label the first of its rows.
        let at = row.checked_sub(self.regions.history.y)? as usize;
        let below_top = self.row_targets[..at]
            .iter()
            .rev()
            .take_while(|t| matches!(t, Target::Scrollback(i, l) if (*i, *l) == (idx, line)))
            .count();
        let row = view.surface.scrollback.get(idx)?;
        let width = self.screen.usable();
        let clicked = line;
        let line = row.click_line(line, width)?;
        // A labelled block answers on its label; the rest is text to select.
        // One without a label has nowhere else, so all of it answers.
        if (clicked != line || below_top > 0) && row.badged(&self.paint, width) {
            return None;
        }
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

    // The transcript cell under the pointer, if it is over the transcript.
    fn point(&self, col: u16, row: u16) -> Option<Point> {
        let h = self.regions.history;
        let row = row.checked_sub(h.y).filter(|r| *r < h.height)?;
        Some(Point {
            row: row as usize,
            col: col.saturating_sub(h.x) as usize,
        })
    }

    pub(super) fn on_press(&mut self, view: &mut View, col: u16, row: u16) {
        self.selection = None;
        // Only transcript rows hold text to select; the pending-calls toggle
        // and the like answer the press itself, as they always did.
        let at = self
            .point(col, row)
            .filter(|_| matches!(self.target_at(row), Some(Target::Scrollback(..))));
        let Some(at) = at else {
            self.press = None;
            self.on_mouse_click(view, col, row);
            return;
        };
        let count = match &self.press {
            Some(p) if p.at == at && p.when.elapsed() < MULTI_CLICK => (p.count % 3) + 1,
            _ => 1,
        };
        self.press = Some(Press {
            at,
            when: std::time::Instant::now(),
            count,
            moved: false,
        });
        let picked = match count {
            2 => self.word_selection(at),
            3 => self.line_selection(at),
            _ => None,
        };
        if let Some(sel) = picked {
            self.selection = Some(sel);
            self.copy_selection(view);
        }
    }

    pub(super) fn on_drag(&mut self, col: u16, row: u16) {
        let h = self.regions.history;
        // Past the region's edge the drag holds at the edge row.
        let row = row.clamp(h.y, h.y + h.height.saturating_sub(1));
        let (Some(head), Some(press)) = (self.point(col, row), self.press.as_mut()) else {
            return;
        };
        if head != press.at {
            press.moved = true;
        }
        if press.moved {
            self.selection = Some(Selection {
                anchor: press.at,
                head,
            });
        }
    }

    pub(super) fn on_release(&mut self, view: &mut View, col: u16, row: u16) {
        let Some(press) = &self.press else {
            return;
        };
        if press.moved {
            self.copy_selection(view);
        } else if press.count == 1 {
            self.on_mouse_click(view, col, row);
        }
    }

    fn word_selection(&self, at: Point) -> Option<Selection> {
        let text = self.drawn.get(at.row)?;
        let (from, to) = select::word_at(text, at.col);
        Some(Selection {
            anchor: Point { col: from, ..at },
            head: Point {
                col: to.saturating_sub(1),
                ..at
            },
        })
    }

    // The whole logical line under `at`: every row wrapped from it.
    fn line_selection(&self, at: Point) -> Option<Selection> {
        let target = *self.row_targets.get(at.row)?;
        let same = |r: &usize| {
            matches!(
                (self.row_targets.get(*r), target),
                (Some(Target::Scrollback(i, l)), Target::Scrollback(ti, tl)) if *i == ti && *l == tl
            )
        };
        let mut first = at.row;
        while first > 0 && same(&(first - 1)) {
            first -= 1;
        }
        let mut last = at.row;
        while same(&(last + 1)) {
            last += 1;
        }
        Some(Selection {
            anchor: Point { row: first, col: 0 },
            head: Point {
                row: last,
                col: usize::MAX - 1,
            },
        })
    }

    // The selection to the clipboard, the bar saying how much went.
    fn copy_selection(&mut self, view: &View) {
        let Some(sel) = self.selection else {
            return;
        };
        let width = self.screen.usable();
        let rows: Vec<Drawn> = self
            .drawn
            .iter()
            .zip(&self.row_targets)
            .map(|(text, target)| {
                let line = match target {
                    Target::Scrollback(idx, line) => Some((*idx, *line)),
                    _ => None,
                };
                let lead = line
                    .and_then(|(idx, l)| view.surface.scrollback.get(idx).map(|r| (r, l)))
                    .and_then(|(row, l)| row.line(l, &self.paint, width).1)
                    .map_or(0, |border| border.width());
                Drawn {
                    text: text.clone(),
                    line,
                    lead,
                }
            })
            .collect();
        let text = select::text(&sel, &rows, width);
        if text.is_empty() {
            return;
        }
        let copied = crossterm::execute!(
            std::io::stdout(),
            crossterm::clipboard::CopyToClipboard::to_clipboard_from(text.as_str())
        );
        self.flash(match copied {
            Ok(()) => format!("copied {}", super::row::count(text.chars().count(), "char")),
            Err(e) => format!("not copied: {e}"),
        });
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
