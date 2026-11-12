//! Where a mouse event landed, and what that row means: the frame's regions,
//! and the row under the pointer.

use pi_store::statusline::Place;
use ratatui::layout::{Constraint, Layout, Position, Rect};

use super::Ui;
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
        let scroll = self.window_mut(view);
        match scroll.top.or(here) {
            Some(mut anchor) => {
                let step = step.min(isize::MAX as usize) as isize;
                anchor.rows = if up {
                    anchor.rows.saturating_sub(step)
                } else {
                    anchor.rows.saturating_add(step)
                };
                scroll.top = Some(anchor);
            }
            None if up => scroll.by = scroll.by.saturating_add(step),
            None => scroll.by = scroll.by.saturating_sub(step),
        }
    }

    // A group window turns by its bar's cells and count alone; the blank rest
    // of that row, and a page's transcript, belong to history.
    pub(super) fn wheel_window(&self, view: &mut View, col: u16, row: u16, up: bool) -> bool {
        if self.reading() {
            return false;
        }
        let Some(Target::Scrollback(idx, line)) = self.target_at(row) else {
            return false;
        };
        // A bar the reader pointed at takes the wheel; one that slid under a
        // still pointer while history scrolled does not.
        if self.pointed_at != Some((idx, line)) {
            return false;
        }
        let width = self.screen.width();
        let Some(target) = view.surface.scrollback.get(idx) else {
            return false;
        };
        // A bar over a window opened whole turns nothing: the wheel is the
        // transcript's there, not a bar's.
        if !target.turns_window() {
            return false;
        }
        if (col as usize) >= target.bar_width(line, &self.paint, width) {
            return false;
        }
        // The bar keeps the wheel even at its ends: a window already at its
        // first or last lines does not hand the turn to the transcript.
        let Some(row) = view.surface.scrollback.get_mut(idx) else {
            return false;
        };
        row.scroll_window(up, 1);
        true
    }

    // The columns a drawn row's border takes before its text.
    pub(super) fn lead(&self, view: &View, at: usize) -> usize {
        if let Some(Target::Text(n)) = self.row_targets.get(at) {
            return self
                .page()
                .and_then(|page| page.get(*n))
                .and_then(|line| line.prefix.as_ref())
                .filter(|prefix| prefix.first.width() < self.screen.width())
                .map_or(0, |p| p.first.width());
        }
        let Some(Target::Scrollback(idx, line)) = self.row_targets.get(at) else {
            return 0;
        };
        let width = self.screen.width();
        view.surface
            .scrollback
            .get(*idx)
            .and_then(|r| r.line(*line, &self.paint, width).1)
            .filter(|border| border.width() < width)
            .map_or(0, |b| b.width())
    }

    // What row of the execution list is at screen row `row`.
    fn command_row_at(&self, row: u16) -> Option<super::call::CommandRow> {
        if self.over_menu(row) {
            return None;
        }
        self.command_rows
            .get(row_in(self.regions.executions, row)?)
            .copied()
    }

    // Whether screen row `row` folds or unfolds the execution list.
    pub(super) fn fold_at(&self, row: u16) -> bool {
        self.command_row_at(row) == Some(super::call::CommandRow::Fold)
    }

    // The open execution whose cut-off lines the row at screen row `row` stands for.
    pub(super) fn earlier_at(&self, row: u16) -> Option<u64> {
        match self.command_row_at(row)? {
            super::call::CommandRow::Earlier(id) => Some(id),
            _ => None,
        }
    }

    // The execution whose row of the list is at screen row `row`.
    pub(super) fn command_at(&self, row: u16) -> Option<u64> {
        let Some(super::call::CommandRow::BackgroundCommand(id)) = self.command_row_at(row) else {
            return None;
        };
        let execution = self
            .executions
            .iter()
            .find(|execution| execution.id == id)?;
        let opens = super::call::background_opens(execution, self.screen.width());
        let open = self.background_open.as_ref().is_some_and(|o| o.id == id);
        (open || opens).then_some(id)
    }

    /// The flash under screen cell (`col`, `row`), and whether its page action
    /// is under that cell.
    pub(super) fn flash_at(&self, col: u16, row: u16) -> Option<bool> {
        let (place, at, hit) = self.flash_hit.as_ref()?;
        let strip = self.regions.strip(*place);
        let r = row_in(strip, row)?;
        let c = col.checked_sub(strip.x)? as usize;
        (r == *at && hit.cols.contains(&c)).then(|| {
            hit.more
                .as_ref()
                .is_some_and(|(_, areas)| areas.action.contains(Position::new(col - strip.x, 0)))
        })
    }

    pub(super) fn target_at(&self, row: u16) -> Option<Target> {
        if self.over_menu(row) {
            return None;
        }
        self.row_targets
            .get(row_in(self.regions.history, row)?)
            .copied()
    }

    pub(super) fn page_command_at(&self, col: u16, row: u16) -> Option<u64> {
        if self.reading() || !self.page_at_pointer(col, row) {
            return None;
        }
        match self.target_at(row) {
            Some(Target::BackgroundResult(_, id)) => Some(id),
            _ => self.earlier_at(row).or_else(|| self.command_at(row)),
        }
    }

    fn more_at_pointer(&self, col: u16, row: u16) -> bool {
        !self.over_menu(row)
            && self
                .more_hits
                .iter()
                .any(|area| area.contains(Position::new(col, row)))
    }

    // A drawn hint, or a row whose own text already says a click opens it.
    fn page_at_pointer(&self, col: u16, row: u16) -> bool {
        self.more_at_pointer(col, row)
            || (!self.over_menu(row)
                && self
                    .page_hits
                    .iter()
                    .any(|area| area.contains(Position::new(col, row))))
    }

    // Whether a click on screen row `row` closes the page: its rule, or the
    // transcript it lies over, which stands back while it is up.
    pub(super) fn closes_page_at(&self, row: u16) -> bool {
        self.read.is_some() && !matches!(self.target_at(row), None | Some(Target::Text(_)))
    }

    // Whether `row` belongs to the list or its heading.
    fn over_menu(&self, row: u16) -> bool {
        row_in(self.regions.overlay(), row).is_some()
    }

    /// The scrollback row and line under the cursor, if the cursor is over
    /// that line's own text.
    pub(super) fn hovered_row(&self, view: &View, col: u16, row: u16) -> Option<(usize, usize)> {
        // Under a page the transcript only stands back: none of it answers.
        if self.read.is_some() {
            return None;
        }
        let target = self.target_at(row)?;
        let Target::Scrollback(idx, line) = target else {
            return None;
        };
        // A drawn block is one line, its label the first row.
        let below_top = *self.row_of.get(row_in(self.regions.history, row)?)?;
        let on_more = self.more_at_pointer(col, row);
        let row = view.surface.scrollback.get(idx)?;
        let width = self.screen.width();
        let clicked = line;
        let line = row.click_line(line, width)?;
        let badged = row.badged(&self.paint, width);
        // A drawn table has no copy badge: its cells are text to select.
        if row.code().is_some() && !badged {
            return None;
        }
        // A labelled block answers on its label; the rest is text to select.
        if (clicked != line || below_top > 0) && badged {
            return None;
        }
        // Excludes the empty rest of the row past the text: the row pointed
        // at, not the line it toggles, which may be narrower.
        let (text, border) = row.line(clicked, &self.paint, width);
        let lead = border
            .filter(|b| b.width() < width)
            .map_or(0, |b| b.width());
        let start = if below_top == 0 {
            row.click_start(clicked, &self.paint, width)
        } else {
            lead
        };
        let end = lead + pi_store::text::width(&screen::plain(&text));
        ((start..end).contains(&(col as usize)) || on_more).then_some((idx, line))
    }

    pub(super) fn on_mouse_move(&mut self, view: &mut View, col: u16, row: u16) {
        self.pointer = Some((col, row));
        self.hovered_scrollback = self.hovered_row(view, col, row);
        self.pointed_at = self.spot_at(row);
    }

    // The scrollback row and line a screen row shows, however it is drawn.
    fn spot_at(&self, row: u16) -> Option<(usize, usize)> {
        match self.target_at(row) {
            Some(Target::Scrollback(idx, line) | Target::Separator(idx, line)) => Some((idx, line)),
            _ => None,
        }
    }

    // Whether screen row `row` is the one that jumps to the bottom.
    pub(super) fn jump_at(&self, row: u16) -> bool {
        let gap = self.regions.gap;
        self.below > 0 && row_in(gap, row).is_some()
    }

    pub(super) fn on_mouse_click(&mut self, view: &mut View, col: u16, row: u16) {
        // A click points at what is under it as plainly as a move does.
        self.pointed_at = self.spot_at(row);
        if self.jump_at(row) {
            self.scroll_view(view, false, super::screen::TOP);
            return;
        }
        if self.fold_at(row) {
            self.background_all = !self.background_all;
            return;
        }
        // Only the explicit page action opens a cut flash.
        if self.flash_at(col, row) == Some(true)
            && let Some((line, _)) = &self.flash
        {
            let whole = ratatui::text::Line::from(line.clone());
            self.open_page(vec![whole.into()]);
            return;
        }
        if self.closes_page_at(row) {
            self.close_page();
            return;
        }
        match self.target_at(row) {
            // Live rows rebuild every frame, so a flag flip is enough.
            Some(Target::PendingTool(row)) => {
                let id = self
                    .tool_rows
                    .get(row)
                    .and_then(Option::as_ref)
                    .map(String::as_str);
                match id.and_then(|id| {
                    view.state
                        .tools
                        .iter_mut()
                        .find(|t| t.id == id && t.printable())
                }) {
                    Some(t) => t.open = !t.open,
                    None => self.live_tools_shown = !self.live_tools_shown,
                }
            }
            Some(Target::Text(n)) => {
                if let Some(code) = self.page_copies(n) {
                    let said = copy_out(&code);
                    self.flash(said);
                }
            }
            _ => {
                let width = self.screen.width();
                let clicked = match self.target_at(row) {
                    Some(Target::Scrollback(_, line)) => line,
                    _ => 0,
                };
                let more = self.more_at_pointer(col, row);
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
                    // A cut's hint under the pointer, or a line too tall for
                    // the window, is read on a page.
                    let screen = self.screen.area().height as usize;
                    let page = (more && row.has_more(clicked, width))
                        .then(|| row.opening(line, &self.paint))
                        .flatten()
                        .or_else(|| row.page_at(line, &self.paint, width, screen));
                    if let Some(page) = page {
                        self.open_page(page);
                        return;
                    }
                    if !row.toggle_at(line, &self.paint, width, screen) {
                        return;
                    }
                    // What changed is under the line clicked, so that line
                    // stays where it was.
                    self.pin(view);
                }
            }
        }
    }

    // The cell under the pointer, if it is over a row text is selected in.
    fn point(&self, col: u16, row: u16) -> Option<Point> {
        if self.over_menu(row) {
            return None;
        }
        (self.regions.readable().into_iter()).find_map(|(r, base)| {
            Some(Point {
                row: base + row_in(r, row)?,
                col: col.saturating_sub(r.x) as usize,
            })
        })
    }

    pub(super) fn on_press(&mut self, view: &mut View, col: u16, row: u16) {
        self.selection = None;
        // Only transcript rows hold text to select; the pending-calls toggle
        // and the like answer the press itself, as they always did.
        let [_, (executions, _), (bar, _)] = self.regions.readable();
        let in_bar = [executions, bar]
            .into_iter()
            .any(|r| row_in(r, row).is_some());
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
        let execution = self.command_at(row);
        // A row that answers a click is clicked, never double-clicked.
        let single = execution.is_some() || self.fold_at(row) || self.earlier_at(row).is_some();
        let count = match &self.press {
            Some(p) if !single && p.at == at && p.when.elapsed() < MULTI_CLICK => (p.count % 3) + 1,
            _ => 1,
        };
        self.press = Some(Press {
            at,
            when: std::time::Instant::now(),
            count,
            moved: false,
            execution,
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
        // Past the edge the drag holds at the edge row; between two stretches,
        // at the last row of the one above.
        let row = (self.regions.readable().into_iter())
            .map(|(r, _)| r)
            .rev()
            .find(|r| r.height > 0 && r.y <= row)
            .map_or(self.regions.history.y, |r| row.min(r.bottom() - 1));
        let (Some(head), Some(press)) = (self.point(col, row), self.press.as_mut()) else {
            return;
        };
        if head != press.at {
            press.moved = true;
        }
        if press.moved {
            self.selection = Some(Selection {
                source: None,
                fragments: Vec::new(),
                anchor: press.at,
                head,
            });
        }
    }

    // A click on a command's row that opens hands back that execution to open.
    pub(super) fn on_release(&mut self, view: &mut View, col: u16, row: u16) -> Option<u64> {
        let press = self.press.as_ref()?;
        if press.moved {
            self.copy_selection(view);
        } else if let Some(id) = press.execution {
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
            source: None,
            fragments: Vec::new(),
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
        let width = self.screen.width();
        let (line, border, units) = match self.row_targets.get(at.row)? {
            Target::Scrollback(idx, i) => {
                let row = view.surface.scrollback.get(*idx)?;
                let (line, border) = row.line(*i, &self.paint, width);
                (line, border, row.units(*i, &self.paint, width))
            }
            Target::Text(n) => {
                let page = self.page()?.get(*n)?;
                (
                    page.line.clone(),
                    page.prefix.as_ref().map(|p| p.first.clone()),
                    page.units.clone(),
                )
            }
            _ => return None,
        };
        if units.is_empty() {
            return None;
        }
        let lead = border
            .as_ref()
            .filter(|border| border.width() < width)
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
        let cell = match self.row_targets.get(at.row)? {
            Target::Scrollback(idx, _) => {
                view.surface
                    .scrollback
                    .get(*idx)?
                    .cell(&self.paint, width, col)
            }
            _ => None,
        };
        let text = cell
            .as_ref()
            .map(|(_, text, _)| text.to_string())
            .unwrap_or_else(|| select::cols(&drawn.concat(), unit.start, unit.end));
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
        let mut fragments = Vec::new();
        if let Some((_, _, ranges)) = &cell {
            for range in ranges {
                let (first_row, first_col) = place(range.start);
                let (last_row, last_col) = place(range.end - 1);
                for (j, row_width) in rows.iter().enumerate().take(last_row + 1).skip(first_row) {
                    if let Some(row) = (at.row + j).checked_sub(k).filter(|row| *row <= last) {
                        let start = if j == first_row { first_col } else { lead };
                        let end = if j == last_row {
                            last_col + 1
                        } else {
                            row_width + lead
                        };
                        fragments.push((row, start..end));
                    }
                }
            }
        }
        let anchor = fragments.first().map_or(anchor, |(row, range)| Point {
            row: *row,
            col: range.start,
        });
        let head = fragments.last().map_or(head, |(row, range)| Point {
            row: *row,
            col: range.end - 1,
        });
        Some((
            Selection {
                anchor,
                head,
                source: cell.and_then(|(id, _, _)| {
                    super::search::Spot::of(*self.row_targets.get(at.row)?)
                        .map(|at| select::Source::Cell { at, id })
                }),
                fragments,
            },
            text,
        ))
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
            source: None,
            fragments: Vec::new(),
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
            source: None,
            fragments: Vec::new(),
            anchor: Point { row: first, col: 0 },
            head: Point {
                row: last,
                col: usize::MAX - 1,
            },
        })
    }

    // The selection to the clipboard.
    fn copy_selection(&mut self, view: &View) {
        let Some(sel) = self.selection.as_ref() else {
            return;
        };
        let width = self.screen.width();
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
        self.copy_text(&select::text(sel, &rows, width));
    }

    // `text` to the clipboard, the bar saying how much went.
    pub(super) fn copy_text(&mut self, text: &str) {
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
    // Trailing whitespace has an ordered history position but no click action.
    Separator(usize, usize),
    // A running call's own line in the live block: a click opens or closes
    // what it printed, or folds the batch when there is nothing to show.
    PendingTool(usize),
    // Completed executions waiting to be delivered in the live block.
    BackgroundResults(usize),
    BackgroundResult(usize, u64),
    Live(usize),
    // Line `n` of the text the reading page shows.
    Text(usize),
    // The rule heading the reading page: a click on it closes the page.
    Rule,
    // Neither: the room over a page's output.
    Gap,
}

// The frame's regions, laid out once and drawn by name. The only place the
// vertical arrangement is stated; everything else reads the rects back.
#[derive(Clone, Copy, Default)]
pub(super) struct Regions {
    // The scrolled transcript, the live region's rows included.
    pub(super) history: Rect,
    // The blank row under it, which says what is below once scrolled up.
    pub(super) gap: Rect,
    // The question the completion list answers, over its rule.
    pub(super) asked: Rect,
    // The row over the completion list's options, parting them from the
    // question or, with none, the transcript.
    pub(super) rule: Rect,
    // The completion list.
    pub(super) menu: Rect,
    // The background commands, over the input line.
    pub(super) executions: Rect,
    // The input line, between the conversation and the bar.
    pub(super) editor: Rect,
    // The row over the input line, when the layout asks for one. A menu
    // covers it while one is open: an option sits beside the input, not
    // above a row of blank.
    pub(super) statusline_top: Rect,
    // The row under it: the checkouts, or the flash that took its row.
    pub(super) statusline: Rect,
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

/// The blank rows between the transcript and whatever is pinned under it —
/// a panel or the bar — so its last line never reads as part of them.
pub(super) const HISTORY_GAP: u16 = 1;

/// One band under the transcript, listed in the order it is laid: the order
/// is also what a short screen keeps, so the input line goes first and the
/// last pane squeezes first.
#[derive(Clone, Copy, Debug)]
pub(super) enum Pane {
    /// The statusline's rows: the ones over the input line, or the ones
    /// under it. One variant, since they differ only in where they sit.
    Statusline { place: Place, rows: usize },
    /// The input line.
    Editor(usize),
    /// A question and the options that answer it, when one is open. It
    /// covers the rows over the input line rather than taking its own: a
    /// list that opened would otherwise move what it lies beside.
    Panel { asked: usize, menu: usize },
    /// The background commands.
    Executions(usize),
}

/// What a panel keeps once the rows are shared out.
#[derive(Clone, Copy, Debug, Default)]
struct Panel {
    asked: usize,
    rule: usize,
    menu: usize,
}

impl Panel {
    fn height(&self) -> usize {
        self.asked + self.rule + self.menu
    }

    /// As many rows as `room` allows: the question first, a rule whenever it
    /// has anything to part and a row under it, the options with what is left.
    fn fitted(asked: usize, menu: usize, room: usize) -> Self {
        let rule = usize::from(room > 1 && asked + menu > 0);
        let asked = asked.min(room.saturating_sub(rule));
        let menu = menu.min(room.saturating_sub(rule + asked));
        Self { asked, rule, menu }
    }
}

// `n` rows of what is left, and what is left after them.
fn take(room: &mut usize, n: usize) -> usize {
    let got = n.min(*room);
    *room -= got;
    got
}

// `row`'s place among `area`'s rows, if it is one of them.
pub(super) fn row_in(area: Rect, row: u16) -> Option<usize> {
    (area.contains(Position::new(area.x, row))).then(|| (row - area.y) as usize)
}

impl Regions {
    /// The row `place` draws on: the one over the input line, or the one
    /// under it.
    pub(super) fn strip(&self, place: Place) -> Rect {
        match place {
            Place::Top => self.statusline_top,
            Place::Bottom => self.statusline,
        }
    }

    /// What sits under the transcript, as many rows as `area` holds: the
    /// panes take their turn in the order `panes` lists them.
    pub(super) fn layout(area: Rect, panes: &[Pane]) -> Self {
        // A row of history and the gap under it stay whatever else is asked
        // for: a conversation with no row of its own has nothing to read.
        let mut room = (area.height as usize).saturating_sub(1 + HISTORY_GAP as usize);
        let (mut bar, mut editor, mut top, mut executions) = (0, 0, 0, 0);
        let (mut asked, mut menu) = (0, 0);
        for pane in panes {
            match *pane {
                Pane::Statusline {
                    place: Place::Bottom,
                    rows,
                } => bar = take(&mut room, rows),
                Pane::Statusline {
                    place: Place::Top,
                    rows,
                } => top = take(&mut room, rows),
                Pane::Editor(n) => editor = take(&mut room, n),
                Pane::Panel {
                    asked: questions,
                    menu: options,
                } => (asked, menu) = (questions, options),
                Pane::Executions(n) => executions = take(&mut room, n),
            }
        }
        // One stack, top to bottom: what a screen shows when the panes above
        // it have taken their rows.
        let [history, gap, executions, statusline_top, editor, statusline] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(HISTORY_GAP),
            Constraint::Length(executions as u16),
            Constraint::Length(top as u16),
            Constraint::Length(editor as u16),
            Constraint::Length(bar as u16),
        ])
        .areas(area);
        // The panel covers the rows just over the input line — the gap and
        // the row above it with them, so an option sits beside the line it
        // completes. At most half of what stands above, so the conversation
        // the question is about still reads around it.
        let above = editor.y.saturating_sub(history.y) as usize;
        let panel = Panel::fitted(asked, menu, above.div_ceil(2));
        let [asked, rule, menu] = match panel.height() {
            0 => [Rect::default(); 3],
            height => {
                let foot = Rect {
                    y: editor.y - height as u16,
                    height: height as u16,
                    ..history
                };
                Layout::vertical([
                    Constraint::Length(panel.asked as u16),
                    Constraint::Length(panel.rule as u16),
                    Constraint::Length(panel.menu as u16),
                ])
                .areas::<3>(foot)
            }
        };
        Self {
            history,
            gap,
            asked,
            rule,
            menu,
            executions,
            statusline_top,
            editor,
            statusline,
        }
    }

    // The list's question, rule and options, whether reserved or overlaid.
    pub(super) fn overlay(&self) -> Rect {
        self.asked.union(self.rule).union(self.menu)
    }

    // The rows text is selected in, top to bottom, each with the index its
    // first row takes among them all.
    pub(super) fn readable(&self) -> [(Rect, usize); 3] {
        let mut base = 0;
        [self.history, self.executions, self.statusline].map(|r| {
            let at = base;
            base += r.height as usize;
            (r, at)
        })
    }
}
