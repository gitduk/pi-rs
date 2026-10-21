//! Where a mouse event landed, and what that row means: the frame's four
//! regions, and the row under the pointer.

use ratatui::layout::{Constraint, Layout, Rect};

use super::Ui;
use super::browse::{Page, TextClick};
use super::screen;
use super::select::{self, Drawn, Point, Selection};
use super::ui::Press;
use super::view::View;

// Presses closer than this on one cell count as one double or triple click.
const MULTI_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

impl Ui {
    // By rows from the top row the last frame drew, so what lands before the
    // next one moves nothing; before any frame, by rows from the bottom.
    pub(super) fn scroll_view(&mut self, view: &mut View, up: bool, step: usize) {
        let here = self.anchor_here(view);
        let (scroll, top) = self.window_mut(view);
        match top.or(here) {
            Some(mut anchor) => {
                let step = step.min(isize::MAX as usize) as isize;
                anchor.rows = if up {
                    anchor.rows.saturating_sub(step)
                } else {
                    anchor.rows.saturating_add(step)
                };
                *top = Some(anchor);
            }
            None if up => *scroll = scroll.saturating_add(step),
            None => *scroll = scroll.saturating_sub(step),
        }
    }

    // The columns a drawn row's border takes before its text.
    pub(super) fn lead(&self, view: &View, at: usize) -> usize {
        let Some(Target::Scrollback(idx, line)) = self.row_targets.get(at) else {
            return 0;
        };
        let width = self.screen.usable();
        view.surface
            .scrollback
            .get(*idx)
            .and_then(|r| r.line(*line, &self.paint, width).1)
            .map_or(0, |b| b.width())
    }

    // What row of the job list is at screen row `row`.
    fn job_row_at(&self, row: u16) -> Option<super::call::JobRow> {
        let bar = self.regions.bar;
        let at = row.checked_sub(bar.y).filter(|r| *r < bar.height)? as usize;
        self.job_rows.get(at).copied()
    }

    // Whether screen row `row` folds or unfolds the job list.
    pub(super) fn fold_at(&self, row: u16) -> bool {
        self.job_row_at(row) == Some(super::call::JobRow::Fold)
    }

    // The open job whose cut-off lines the row at screen row `row` stands for.
    pub(super) fn earlier_at(&self, row: u16) -> Option<u64> {
        match self.job_row_at(row)? {
            super::call::JobRow::Earlier(id) => Some(id),
            _ => None,
        }
    }

    // The job whose row of the bar is at screen row `row`.
    pub(super) fn job_at(&self, row: u16) -> Option<u64> {
        let Some(super::call::JobRow::Job(id)) = self.job_row_at(row) else {
            return None;
        };
        let job = self.jobs.iter().find(|job| job.id == id)?;
        let opens = super::call::job_opens(job, self.screen.usable());
        (self.job_open == Some(id) || opens).then_some(id)
    }

    /// The flash under screen cell (`col`, `row`), and whether the bar cut
    /// it: the side of the bar row that holds it.
    pub(super) fn flash_at(&self, col: u16, row: u16) -> Option<bool> {
        let (at, cols, cut) = self.flash_hit.as_ref()?;
        let bar = self.regions.bar;
        let r = row.checked_sub(bar.y).filter(|r| *r < bar.height)? as usize;
        let c = col.checked_sub(bar.x)? as usize;
        (r == *at && cols.contains(&c)).then_some(*cut)
    }

    pub(super) fn target_at(&self, row: u16) -> Option<Target> {
        if self.over_menu(row) {
            return None;
        }
        self.row_targets
            .get(row.checked_sub(self.regions.history.y)? as usize)
            .copied()
    }

    // Whether `row` is under the list, which lies over the history there.
    fn over_menu(&self, row: u16) -> bool {
        let menu = self.regions.menu;
        (menu.y..menu.y + menu.height).contains(&row)
    }

    /// The scrollback row and line under the cursor, if the cursor is over
    /// that line's own text.
    pub(super) fn hovered_row(&self, view: &View, col: u16, row: u16) -> Option<(usize, usize)> {
        let target = self.target_at(row)?;
        let Target::Scrollback(idx, line) = target else {
            return None;
        };
        // A drawn block is one line, its label the first row.
        let below_top = *self
            .row_of
            .get(row.checked_sub(self.regions.history.y)? as usize)?;
        let row = view.surface.scrollback.get(idx)?;
        let width = self.screen.usable();
        let clicked = line;
        let line = row.click_line(line, width)?;
        // A labelled block answers on its label; the rest is text to select.
        // One without a label has nowhere else, so all of it answers.
        if (clicked != line || below_top > 0) && row.badged(&self.paint, width) {
            return None;
        }
        // Excludes the empty rest of the row past the text: the row pointed
        // at, not the line it toggles, which may be narrower.
        let (text, border) = row.line(clicked, &self.paint, width);
        let start = border.map_or(0, |b| b.width());
        (start..start + pi_store::text::width(&screen::plain(&text)))
            .contains(&(col as usize))
            .then_some((idx, line))
    }

    pub(super) fn on_mouse_move(&mut self, view: &mut View, col: u16, row: u16) {
        self.pointer = Some((col, row));
        self.hovered_scrollback = self.hovered_row(view, col, row);
    }

    // Whether screen row `row` is the one that jumps to the bottom.
    pub(super) fn jump_at(&self, row: u16) -> bool {
        let gap = self.regions.gap;
        self.below > 0 && (gap.y..gap.bottom()).contains(&row)
    }

    pub(super) fn on_mouse_click(&mut self, view: &mut View, col: u16, row: u16) {
        if self.jump_at(row) {
            self.scroll_view(view, false, super::screen::TOP);
            return;
        }
        if self.fold_at(row) {
            self.jobs_all = !self.jobs_all;
            return;
        }
        if self.earlier_at(row).is_some() {
            let whole = pi_store::listing::Listing::say(self.job_body.lines().map(str::to_string));
            self.open_slash(whole);
            return;
        }
        // A cut flash opens whole, as an answer the click asked for.
        if self.flash_at(col, row) == Some(true)
            && let Some((line, _)) = &self.flash
        {
            let whole = pi_store::listing::Listing::say([line.clone()]);
            self.open_slash(whole);
            return;
        }
        match self.target_at(row) {
            // Live rows rebuild every frame, so a flag flip is enough.
            Some(Target::PendingTools(_)) => self.live_tools_shown = !self.live_tools_shown,
            Some(Target::Text(n)) => match self.text_click(n) {
                Some(TextClick::Copy(code)) => {
                    let said = copy_out(&code);
                    self.flash(said);
                }
                Some(TextClick::Close) => {
                    self.close_page();
                }
                None => {}
            },
            _ => {
                let width = self.screen.usable();
                let hit = self.hovered_row(view, col, row);
                if let Some((idx, code)) =
                    hit.and_then(|(idx, _)| Some((idx, view.surface.scrollback.get(idx)?.code()?)))
                {
                    let said = copy_out(code);
                    let row = &mut view.surface.scrollback[idx];
                    if row.badged(&self.paint, width) {
                        row.say_copied(said);
                        self.copied = Some(std::time::Instant::now());
                    } else {
                        self.flash(said);
                    }
                } else if let Some((idx, line)) = hit
                    && let Some(row) = view.surface.scrollback.get_mut(idx)
                {
                    // Too long or too wide to read in place: read on a page of
                    // its own, the line clicked heading it, left folded here.
                    if self.read.is_none()
                        && let Some((head, body)) = row.opening(line, &self.paint, width)
                        && (body.len() > self.regions.history.height as usize / 2
                            || row.clipped(width))
                    {
                        self.preview(std::iter::once(head.into()).chain(body).collect());
                        return;
                    }
                    if !row.toggle_at(line, width) {
                        return;
                    }
                    // What changed is under the line clicked, so that line
                    // stays where it was.
                    self.pin(view);
                }
            }
        }
    }

    // The cell under the pointer, if it is over the transcript or the bar:
    // the bar's rows count on from the history's full height.
    fn point(&self, col: u16, row: u16) -> Option<Point> {
        if self.over_menu(row) {
            return None;
        }
        let (h, b) = (self.regions.history, self.regions.bar);
        let row = match row.checked_sub(h.y).filter(|r| *r < h.height) {
            Some(r) => r,
            None => h.height + row.checked_sub(b.y).filter(|r| *r < b.height)?,
        };
        Some(Point {
            row: row as usize,
            col: col.saturating_sub(h.x) as usize,
        })
    }

    pub(super) fn on_press(&mut self, view: &mut View, col: u16, row: u16) {
        self.selection = None;
        // Only transcript rows hold text to select; the pending-calls toggle
        // and the like answer the press itself, as they always did.
        let in_bar = (self.regions.bar.y..self.regions.bar.bottom()).contains(&row);
        let at = self.point(col, row).filter(|_| {
            in_bar
                || matches!(
                    self.target_at(row),
                    Some(Target::Scrollback(..) | Target::Text(_))
                )
        });
        let Some(at) = at else {
            self.press = None;
            self.on_mouse_click(view, col, row);
            return;
        };
        let job = self.job_at(row);
        // A row that answers a click is clicked, never double-clicked.
        let single = job.is_some() || self.fold_at(row) || self.earlier_at(row).is_some();
        let count = match &self.press {
            Some(p) if !single && p.at == at && p.when.elapsed() < MULTI_CLICK => (p.count % 3) + 1,
            _ => 1,
        };
        self.press = Some(Press {
            at,
            when: std::time::Instant::now(),
            count,
            moved: false,
            job,
        });
        if count == 2
            && let Some((sel, text)) = self.unit_selection(view, at)
        {
            self.selection = Some(sel);
            self.copy_text(&text);
            return;
        }
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
        let (h, b) = (self.regions.history, self.regions.bar);
        // Past the edge the drag holds at the edge row; between the two, at
        // the history's last.
        let last = if b.height > 0 {
            b.bottom() - 1
        } else {
            h.bottom().saturating_sub(1)
        };
        let row = row.clamp(h.y, last);
        let row = if row >= h.bottom() && row < b.y {
            h.bottom().saturating_sub(1)
        } else {
            row
        };
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

    // A click on a job's row that opens hands back that job to open.
    pub(super) fn on_release(&mut self, view: &mut View, col: u16, row: u16) -> Option<u64> {
        let press = self.press.as_ref()?;
        if press.moved {
            self.copy_selection(view);
        } else if let Some(id) = press.job {
            return Some(id);
        } else if press.count == 1 {
            self.on_mouse_click(view, col, row);
        }
        None
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

    // The stretch under `at` that reads as one — code, a link — and its
    // text, found in the line it belongs to however that wrapped.
    pub(super) fn unit_selection(&self, view: &View, at: Point) -> Option<(Selection, String)> {
        let width = self.screen.usable();
        let (line, border, units) = match self.row_targets.get(at.row)? {
            Target::Scrollback(idx, i) => {
                let row = view.surface.scrollback.get(*idx)?;
                let (line, border) = row.line(*i, &self.paint, width);
                (line, border, row.units(*i, &self.paint, width))
            }
            Target::Text(n) => {
                let Some(Page::Preview(lines)) = self.page() else {
                    return None;
                };
                let page = lines.get(*n)?;
                (page.line.clone(), None, page.units.clone())
            }
            _ => return None,
        };
        if units.is_empty() {
            return None;
        }
        let lead = border
            .as_ref()
            .map_or(0, |b| pi_store::text::width(&screen::plain(b)));
        // Read off the rows as laid, tabs as spaces and escapes gone, so the
        // columns counted and the text taken are the same characters.
        let drawn: Vec<String> = (screen::wrap(border.as_ref(), &line, width).iter())
            .map(|r| select::cols(&screen::plain(r), lead, usize::MAX))
            .collect();
        let rows: Vec<usize> = drawn.iter().map(|r| pi_store::text::width(r)).collect();
        let k = *self.row_of.get(at.row)?;
        let col = rows.iter().take(k).sum::<usize>() + at.col.checked_sub(lead)?;
        let unit = crate::unit::at(&units, col)?;
        let text = select::cols(&drawn.concat(), unit.start, unit.end);
        // Cell `c` of the line where it is drawn: which of its rows, what column.
        let place = |mut c: usize| {
            let mut j = 0;
            while j + 1 < rows.len() && c >= rows[j] {
                c -= rows[j];
                j += 1;
            }
            (j, c + lead)
        };
        // Rows past the window's edges are held at the edge row.
        let last = (self.regions.history.height as usize).saturating_sub(1);
        let point = |(j, col): (usize, usize), edge: Point| match (at.row + j).checked_sub(k) {
            Some(row) if row <= last => Point { row, col },
            _ => edge,
        };
        let anchor = point(place(unit.start), Point { row: 0, col: lead });
        let head = point(
            place(unit.end - 1),
            Point {
                row: last,
                col: usize::MAX - 1,
            },
        );
        Some((Selection { anchor, head }, text))
    }

    // Whether drawn rows `a` and `b` are wraps of one logical line.
    fn same_line(&self, a: usize, b: usize) -> bool {
        matches!(
            (self.row_targets.get(a), self.row_targets.get(b)),
            (Some(Target::Scrollback(i, l)), Some(Target::Scrollback(j, m))) if i == j && l == m
        )
    }

    // The whole logical line under `at`: every row wrapped from it.
    fn line_selection(&self, at: Point) -> Option<Selection> {
        let whole = Selection {
            anchor: Point {
                row: at.row,
                col: 0,
            },
            head: Point {
                row: at.row,
                col: usize::MAX - 1,
            },
        };
        // A bar row is a line of its own.
        if self.row_targets.get(at.row).is_none() {
            return Some(whole);
        }
        let mut first = at.row;
        while first > 0 && self.same_line(first - 1, at.row) {
            first -= 1;
        }
        let mut last = at.row;
        while self.same_line(at.row, last + 1) {
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

    // The selection to the clipboard.
    fn copy_selection(&mut self, view: &View) {
        let Some(sel) = self.selection else {
            return;
        };
        let width = self.screen.usable();
        let rows: Vec<Drawn> = (self.drawn.iter().enumerate())
            .map(|(at, text)| Drawn {
                text: text.clone(),
                line: match self.row_targets.get(at) {
                    Some(Target::Scrollback(idx, line)) => Some((*idx, *line)),
                    _ => None,
                },
                lead: self.lead(view, at),
            })
            .collect();
        self.copy_text(&select::text(&sel, &rows, width));
    }

    // `text` to the clipboard, the bar saying how much went.
    fn copy_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let copied = crossterm::execute!(
            std::io::stdout(),
            crossterm::clipboard::CopyToClipboard::to_clipboard_from(text)
        );
        self.flash(match copied {
            Ok(()) => format!("copied {}", super::row::count(text.chars().count(), "char")),
            Err(e) => format!("not copied: {e}"),
        });
    }

    // A page keeps 4 rows of context at the edge, the way the upstream pi
    // TUI does (`Math.max(1, viewportHeight - 4)`).
    pub(super) fn page_scroll_step(&self) -> usize {
        (self.regions.history.height as usize)
            .saturating_sub(4)
            .max(1)
    }

    pub(super) fn half_scroll_step(&self) -> usize {
        (self.regions.history.height as usize / 2).max(1)
    }
}

// What block a rendered history row belongs to, for click handling.
#[derive(Clone, Copy, Debug)]
pub(super) enum Target {
    Scrollback(usize, usize),
    // Row `i` of the live block: a pending call, a click on it opening or
    // closing the batch, or the rest.
    PendingTools(usize),
    Live(usize),
    // Line `n` of the text the reading page shows.
    Text(usize),
}

// The frame's four regions, laid out once and drawn by name. The only place
// the vertical arrangement is stated; everything else reads the rects back.
#[derive(Clone, Copy, Default)]
pub(super) struct Regions {
    // The scrolled transcript, the live region's rows included.
    pub(super) history: Rect,
    // The blank row under it, which says what is below once scrolled up.
    pub(super) gap: Rect,
    // The completion list.
    pub(super) menu: Rect,
    // The bar: the checkouts, or the flash that took its row.
    pub(super) bar: Rect,
    // The input line, pinned to the bottom.
    pub(super) editor: Rect,
}

// `code` to the clipboard, and what the screen says of it.
fn copy_out(code: &str) -> String {
    let copied = crossterm::execute!(
        std::io::stdout(),
        crossterm::clipboard::CopyToClipboard::to_clipboard_from(code)
    );
    match copied {
        Ok(()) => format!("copied {}", super::row::count(code.lines().count(), "line")),
        Err(e) => format!("not copied: {e}"),
    }
}

pub(super) fn at_row_name(path: &str, dir: bool) -> String {
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path);
    if dir {
        format!("{name}/")
    } else {
        name.to_string()
    }
}

/// The blank rows between the transcript and whatever is pinned under it —
/// a panel or the bar — so its last line never reads as part of them.
pub(super) const HISTORY_GAP: u16 = 1;

// The most rows a list over the editor shows; the rest scroll.
const MENU_ROWS: usize = 10;

impl Regions {
    /// The rows each part asks for, as many as `area` holds: input last, so
    /// the caret sits on the bottom row and the bar reads as history's edge.
    pub(super) fn layout(area: Rect, menu: usize, bar: usize, editor: usize) -> Self {
        // A short screen takes from the editor, then the bar; a row of
        // history and a row to type on stay.
        let mut left = (area.height as usize).saturating_sub(1 + HISTORY_GAP as usize);
        let bar = bar.min(left.saturating_sub(1));
        left -= bar;
        let editor = editor.min(left);
        left -= editor;
        let chunks = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(HISTORY_GAP),
            Constraint::Length(bar as u16),
            Constraint::Length(editor as u16),
        ])
        .split(area);
        // The list lies over the rows above the bar, as a command's output
        // does: nothing under it moves as it grows and shrinks. A few rows,
        // and half at most, so what it moves the view to stays in sight.
        let menu = menu.min(MENU_ROWS).min(left.div_ceil(2)) as u16;
        Self {
            history: chunks[0],
            gap: chunks[1],
            menu: Rect {
                y: chunks[2].y - menu,
                height: menu,
                ..chunks[2]
            },
            bar: chunks[2],
            editor: chunks[3],
        }
    }
}
