//! Where a mouse event landed, and what that row means: the frame's four
//! regions, and the row under the pointer.

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
        let width = self.screen.width();
        view.surface
            .scrollback
            .get(*idx)
            .and_then(|r| r.line(*line, &self.paint, width).1)
            .map_or(0, |b| b.width())
    }

    // What row of the job list is at screen row `row`.
    fn job_row_at(&self, row: u16) -> Option<super::call::JobRow> {
        if self.over_menu(row) {
            return None;
        }
        self.job_rows.get(row_in(self.regions.jobs, row)?).copied()
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

    // The job whose row of the list is at screen row `row`.
    pub(super) fn job_at(&self, row: u16) -> Option<u64> {
        let Some(super::call::JobRow::Job(id)) = self.job_row_at(row) else {
            return None;
        };
        let job = self.jobs.iter().find(|job| job.id == id)?;
        let opens = super::call::job_opens(job, self.screen.width());
        let open = self.job_open.as_ref().is_some_and(|o| o.id == id);
        (open || opens).then_some(id)
    }

    /// The flash under screen cell (`col`, `row`), and whether the bar cut
    /// it: the side of the bar row that holds it.
    pub(super) fn flash_at(&self, col: u16, row: u16) -> Option<bool> {
        let (at, cols, cut) = self.flash_hit.as_ref()?;
        let r = row_in(self.regions.bar, row)?;
        let c = col.checked_sub(self.regions.bar.x)? as usize;
        (r == *at && cols.contains(&c)).then_some(*cut)
    }

    pub(super) fn target_at(&self, row: u16) -> Option<Target> {
        if self.over_menu(row) {
            return None;
        }
        self.row_targets
            .get(row_in(self.regions.history, row)?)
            .copied()
    }

    // Whether a click on screen row `row` closes the page: its rule, or the
    // transcript it lies over, which stands back while it is up.
    pub(super) fn closes_page_at(&self, row: u16) -> bool {
        self.read.is_some() && !matches!(self.target_at(row), None | Some(Target::Text(_)))
    }

    // Whether `row` is under the list, which lies over the history.
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
        let row = view.surface.scrollback.get(idx)?;
        let width = self.screen.width();
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
        self.below > 0 && row_in(gap, row).is_some()
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
        // The page shows the whole of it, so the job folds: its tail twice
        // over would read as two outputs.
        if let Some(id) = self.earlier_at(row) {
            let open = self.job_open.take();
            let job = self.jobs.iter().find(|j| j.id == id);
            if let (Some(open), Some(job)) = (open, job) {
                self.open_page(super::call::job_page(job, &open.body, &self.paint));
            }
            return;
        }
        // A cut flash opens whole on the page.
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
            Some(Target::PendingTools(_)) => self.live_tools_shown = !self.live_tools_shown,
            Some(Target::Text(n)) => {
                if let Some(code) = self.page_copies(n) {
                    let said = copy_out(&code);
                    self.flash(said);
                }
            }
            _ => {
                let width = self.screen.width();
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
                    // its own, left folded here. A lone line opens only cut.
                    let half = (self.regions.history.height as usize / 2).max(1);
                    if self.read.is_none()
                        && let Some(page) = row.opening(line, &self.paint)
                        && (page.len() > half || row.clipped(width))
                    {
                        self.open_page(page);
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
        let [_, (jobs, _), (bar, _)] = self.regions.readable();
        let in_bar = [jobs, bar].into_iter().any(|r| row_in(r, row).is_some());
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
        let width = self.screen.width();
        let (line, border, units) = match self.row_targets.get(at.row)? {
            Target::Scrollback(idx, i) => {
                let row = view.surface.scrollback.get(*idx)?;
                let (line, border) = row.line(*i, &self.paint, width);
                (line, border, row.units(*i, &self.paint, width))
            }
            Target::Text(n) => {
                let page = self.page()?.get(*n)?;
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
    // The rule heading the reading page: a click on it closes the page.
    Rule,
    // Neither: the room over a page's output.
    Gap,
}

// The frame's four regions, laid out once and drawn by name. The only place
// the vertical arrangement is stated; everything else reads the rects back.
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
    // The background jobs, over the input line.
    pub(super) jobs: Rect,
    // The input line, between the conversation and the bar.
    pub(super) editor: Rect,
    // The bar: the checkouts, or the flash that took its row; the bottom.
    pub(super) bar: Rect,
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

// `row`'s place among `area`'s rows, if it is one of them.
pub(super) fn row_in(area: Rect, row: u16) -> Option<usize> {
    (area.contains(Position::new(area.x, row))).then(|| (row - area.y) as usize)
}

impl Regions {
    /// The rows each part asks for, as many as `area` holds: the input line
    /// parts the conversation, jobs and all, from the bar under it. A `page`
    /// up is read over all of the conversation, so the jobs get no rows.
    pub(super) fn layout(
        area: Rect,
        page: bool,
        asked: usize,
        menu: usize,
        jobs: usize,
        bar: usize,
        editor: usize,
    ) -> Self {
        // A short screen takes from the editor, then the jobs, then the bar;
        // a row of history and a row to type on stay.
        let mut left = (area.height as usize).saturating_sub(1 + HISTORY_GAP as usize);
        let bar = bar.min(left.saturating_sub(1));
        left -= bar;
        let jobs = if page {
            0
        } else {
            jobs.min(left.saturating_sub(1))
        };
        left -= jobs;
        let editor = editor.min(left);
        left -= editor;
        let chunks = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(HISTORY_GAP),
            Constraint::Length(jobs as u16),
            Constraint::Length(editor as u16),
            Constraint::Length(bar as u16),
        ])
        .split(area);
        // The list lies over the jobs and the rows above them, next to the
        // line it completes: nothing under it moves as it grows and shrinks.
        // Half the history at most, so what it moves the view to stays in sight.
        let shown = (asked + menu).min(jobs + left.div_ceil(2));
        let asked = asked.min(shown);
        let above = chunks[0].union(chunks[2]);
        let [_, asked, rule, menu] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(asked as u16),
            Constraint::Length(u16::from(shown > 0)),
            Constraint::Length((shown - asked) as u16),
        ])
        .areas(above);
        Self {
            history: chunks[0],
            gap: chunks[1],
            asked,
            rule,
            menu,
            jobs: chunks[2],
            editor: chunks[3],
            bar: chunks[4],
        }
    }

    // The rows the completion list lies over: its question, rule and options.
    pub(super) fn overlay(&self) -> Rect {
        self.asked.union(self.rule).union(self.menu)
    }

    // The rows text is selected in, top to bottom, each with the index its
    // first row takes among them all.
    pub(super) fn readable(&self) -> [(Rect, usize); 3] {
        let mut base = 0;
        [self.history, self.jobs, self.bar].map(|r| {
            let at = base;
            base += r.height as usize;
            (r, at)
        })
    }
}
